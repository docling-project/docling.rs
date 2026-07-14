// Minimal smoke test exercising every part of the binding: sync + async
// convert, in-memory bytes, JSON output, the reusable class, streaming, and the
// format helpers. Run with `node test/smoke.mjs` (or `bun test/smoke.mjs`).
//
// Exits non-zero on the first failed assertion, so it doubles as a CI check.

import assert from 'node:assert/strict'
import {
  checkDependencies,
  chunk,
  chunkAsync,
  chunkDocument,
  chunkDocumentAsync,
  chunkFileAsync,
  convert,
  convertFile,
  convertFileAsync,
  DocumentConverter,
  formatFromName,
  Pipeline,
  streamChunks,
  streamDocumentChunks,
  streamFileMarkdown,
  supportedFormats,
} from '../index.js'

let passed = 0
const check = (name, fn) => {
  return Promise.resolve()
    .then(fn)
    .then(() => {
      passed++
      console.log(`  ok  ${name}`)
    })
    .catch((err) => {
      console.error(`fail  ${name}\n      ${err.message}`)
      process.exitCode = 1
      throw err
    })
}

const MD = '# Title\n\nHello **world**.\n\n- one\n- two\n'

async function main() {
  await check('supportedFormats lists md and pdf', () => {
    const formats = supportedFormats()
    assert.ok(formats.includes('md'))
    assert.ok(formats.includes('pdf'))
  })

  await check('formatFromName detects extensions', () => {
    assert.equal(formatFromName('report.pdf'), 'pdf')
    assert.equal(formatFromName('page.html'), 'html')
    assert.equal(formatFromName('mystery.zzz'), null)
  })

  await check('convert (bytes) → Markdown round-trips', () => {
    const res = convert({ name: 'doc', data: Buffer.from(MD), format: 'md' })
    assert.equal(res.status, 'success')
    assert.equal(res.format, 'md')
    assert.match(res.content, /# Title/)
    assert.match(res.content, /Hello/)
  })

  await check('convert (bytes) → JSON is docling-core wire format', () => {
    const res = convert({ name: 'doc', data: Buffer.from(MD), format: 'md' }, { to: 'json' })
    const doc = JSON.parse(res.content)
    assert.equal(doc.schema_name, 'DoclingDocument')
    assert.ok(Array.isArray(doc.texts))
  })

  await check('format inferred from name when omitted', () => {
    const res = convert({ name: 'notes.md', data: Buffer.from(MD) })
    assert.equal(res.format, 'md')
  })

  await check('DocumentConverter class is reusable', () => {
    const converter = new DocumentConverter({ strict: true })
    const a = converter.convert({ name: 'a.md', data: Buffer.from('# A\n') })
    const b = converter.convert({ name: 'b.md', data: Buffer.from('# B\n') })
    assert.match(a.content, /# A/)
    assert.match(b.content, /# B/)
  })

  await check('allowedFormats rejects other formats', () => {
    const converter = new DocumentConverter({ allowedFormats: ['csv'] })
    assert.throws(() => converter.convert({ name: 'x.md', data: Buffer.from(MD) }))
  })

  await check('unknown format string is rejected', () => {
    assert.throws(() => convert({ name: 'x', data: Buffer.from(MD), format: 'nope' }))
  })

  // --- chunking --------------------------------------------------------------

  const CHUNK_MD = '# Guide\n\n## Setup\n\nInstall the tools.\n\n- clone\n- build\n\n## Usage\n\nRun it.\n'

  await check('chunk (hierarchical) carries heading paths and doc items', () => {
    const chunks = chunk({ name: 'guide.md', data: Buffer.from(CHUNK_MD) })
    assert.ok(chunks.length >= 3)
    const setup = chunks.find((c) => c.text.includes('Install'))
    assert.deepEqual(setup.headings, ['Guide', 'Setup'])
    assert.ok(setup.docItems.length >= 1)
    assert.match(setup.docItems[0], /^#\//)
    assert.equal(setup.contextualized, 'Guide\nSetup\nInstall the tools.')
    const list = chunks.find((c) => c.text.includes('clone'))
    assert.equal(list.text, '- clone\n- build')
  })

  await check('chunkAsync resolves off the event loop', async () => {
    const chunks = await chunkAsync({ name: 'guide.md', data: Buffer.from(CHUNK_MD) })
    assert.ok(chunks.length >= 3)
  })

  await check('chunkDocument chunks a converted JSON document', async () => {
    const res = convert({ name: 'guide.md', data: Buffer.from(CHUNK_MD) }, { to: 'json' })
    const sync = chunkDocument(res.content)
    const async_ = await chunkDocumentAsync(res.content)
    assert.deepEqual(async_, sync)
    assert.ok(sync.some((c) => c.text.includes('Install')))
  })

  await check('streamChunks yields the same chunks as chunk, one at a time', async () => {
    const buffered = chunk({ name: 'guide.md', data: Buffer.from(CHUNK_MD) })
    const streamed = []
    for await (const c of streamChunks({ name: 'guide.md', data: Buffer.from(CHUNK_MD) })) {
      streamed.push(c)
    }
    assert.deepEqual(streamed, buffered)
  })

  await check('streamChunks supports early break', async () => {
    let first = null
    for await (const c of streamChunks({ name: 'guide.md', data: Buffer.from(CHUNK_MD) })) {
      first = c
      break // abandoning the generator cancels the background chunking
    }
    assert.ok(first && typeof first.text === 'string')
  })

  await check('streamDocumentChunks streams a converted JSON document', async () => {
    const res = convert({ name: 'guide.md', data: Buffer.from(CHUNK_MD) }, { to: 'json' })
    const streamed = []
    for await (const c of streamDocumentChunks(res.content)) streamed.push(c)
    assert.deepEqual(streamed, chunkDocument(res.content))
  })

  await check('streamChunks surfaces conversion errors', async () => {
    await assert.rejects(async () => {
      for await (const _ of streamChunks({ name: 'x', data: Buffer.from(MD), format: 'nope' })) {
        void _
      }
    })
  })

  await check('hybrid without any tokenizer errors with the download hint', () => {
    // No explicit path and no models/chunk/tokenizer.json in this test cwd.
    assert.throws(
      () => chunk({ name: 'g.md', data: Buffer.from(CHUNK_MD) }, { chunker: 'hybrid' }),
      /download_dependencies|tokenizer/,
    )
  })

  await check('unknown chunker name is rejected', () => {
    assert.throws(
      () => chunk({ name: 'g.md', data: Buffer.from(CHUNK_MD) }, { chunker: 'semantic' }),
      /unknown chunker/,
    )
  })

  // Hybrid end-to-end only when a tokenizer.json is available (repo checkout).
  const { existsSync } = await import('node:fs')
  const TOKENIZER = new URL('../../../tests/data/chunks/tokenizer.json', import.meta.url).pathname
  if (existsSync(TOKENIZER)) {
    await check('hybrid chunker splits against the token budget', async () => {
      const long = '# Doc\n\n' + Array.from({ length: 40 }, (_, i) => `Sentence number ${i} padding words here.`).join(' ') + '\n'
      const hier = chunk({ name: 'l.md', data: Buffer.from(long) })
      const hybrid = await chunkAsync(
        { name: 'l.md', data: Buffer.from(long) },
        { chunker: 'hybrid', tokenizer: TOKENIZER, maxTokens: 64 },
      )
      assert.ok(hybrid.length > hier.length, `expected split: hybrid ${hybrid.length} vs hierarchical ${hier.length}`)
      assert.deepEqual(hybrid[0].headings, ['Doc'])
    })
    await check('hybrid picks up models/chunk/tokenizer.json by default', async () => {
      const { mkdirSync, copyFileSync, mkdtempSync: mktemp } = await import('node:fs')
      const { tmpdir: osTmp } = await import('node:os')
      const { join: joinPath } = await import('node:path')
      const home = mktemp(joinPath(osTmp(), 'fw-chunk-'))
      mkdirSync(joinPath(home, 'models', 'chunk'), { recursive: true })
      copyFileSync(TOKENIZER, joinPath(home, 'models', 'chunk', 'tokenizer.json'))
      const prevCwd = process.cwd()
      process.chdir(home) // deps.js resolves the install home from the cwd
      try {
        const chunks = chunk(
          { name: 'g.md', data: Buffer.from(CHUNK_MD) },
          { chunker: 'hybrid', maxTokens: 64 },
        )
        // Undersized same-heading peers merge: Setup's paragraph + list
        // become one chunk, so hybrid yields fewer chunks than hierarchical.
        assert.ok(chunks.length >= 2)
        assert.ok(chunks.some((c) => c.text.includes('Install') && c.text.includes('clone')))
      } finally {
        process.chdir(prevCwd)
      }
    })
  } else {
    console.log('  --  tokenizer.json not found; skipping hybrid end-to-end check')
  }

  // --- ML dependency guards (models not installed in this test env) ---------

  await check('checkDependencies reports status without downloading', () => {
    const status = checkDependencies()
    assert.equal(typeof status.ready, 'boolean')
    assert.equal(typeof status.pdfium, 'boolean')
    assert.ok(Array.isArray(status.missing))
  })

  // These assume the ML models are NOT installed (true on a fresh CI checkout).
  const depsInstalled = checkDependencies().ready
  if (!depsInstalled) {
    await check('convert PDF (sync) throws pointing at download_dependencies.sh', () => {
      assert.throws(
        () => convert({ name: 'doc.pdf', data: Buffer.from('%PDF-1.4') }),
        /download_dependencies\.sh/,
      )
    })

    await check('convertFileAsync PDF rejects (not a sync throw)', async () => {
      await assert.rejects(convertFileAsync('missing.pdf'), /download_dependencies\.sh/)
    })

    await check('image bytes are guarded too', () => {
      assert.throws(() => convert({ name: 'scan.png', data: Buffer.from([0]) }), /download_dependencies\.sh/)
    })

    await check('Pipeline convertFile is guarded', () => {
      const pipe = new Pipeline()
      assert.throws(() => pipe.convertFile('x.pdf'), /download_dependencies\.sh/)
    })

    await check('Pipeline convertFileAsync rejects (not a sync throw)', async () => {
      const pipe = new Pipeline()
      await assert.rejects(pipe.convertFileAsync('x.pdf'), /download_dependencies\.sh/)
    })

    await check('Pipeline streamFileMarkdown rejects on iteration', async () => {
      const pipe = new Pipeline()
      await assert.rejects(pipe.streamFileMarkdown('x.pdf').next(), /download_dependencies\.sh/)
    })
  } else {
    console.log('  --  ML deps installed; skipping guard checks')
  }

  // File-based sync + async + streaming, using a temp Markdown file.
  const { writeFileSync, mkdtempSync } = await import('node:fs')
  const { tmpdir } = await import('node:os')
  const { join } = await import('node:path')
  const dir = mkdtempSync(join(tmpdir(), 'fw-smoke-'))
  const file = join(dir, 'doc.md')
  writeFileSync(file, MD)

  await check('convertFile (sync) reads from disk', () => {
    const res = convertFile(file)
    assert.match(res.content, /# Title/)
    assert.equal(res.inputName, 'doc')
  })

  await check('convertFileAsync returns a Promise', async () => {
    const res = await convertFileAsync(file, { to: 'json' })
    assert.equal(JSON.parse(res.content).schema_name, 'DoclingDocument')
  })

  await check('streamFileMarkdown yields chunks equal to buffered output', async () => {
    let streamed = ''
    for await (const chunk of streamFileMarkdown(file)) {
      streamed += chunk
    }
    assert.equal(streamed, convertFile(file).content)
    assert.ok(streamed.length > 0)
  })

  // Warm-pipeline async + streaming, only when the ML deps are on disk (they
  // are in the repo dev environment; a fresh CI checkout skips these).
  if (depsInstalled) {
    const pdf = new URL('../../../tests/data/pdf/sources/code_and_formula.pdf', import.meta.url)
      .pathname
    const { existsSync } = await import('node:fs')
    if (existsSync(pdf)) {
      const pipe = new Pipeline()

      await check('Pipeline convertFileAsync resolves with the buffered output', async () => {
        const buffered = pipe.convertFile(pdf)
        const res = await pipe.convertFileAsync(pdf)
        assert.equal(res.status, 'success')
        assert.equal(res.content, buffered.content)
      })

      await check('Pipeline convertFileAsync to JSON', async () => {
        const res = await pipe.convertFileAsync(pdf, { to: 'json' })
        assert.equal(JSON.parse(res.content).schema_name, 'DoclingDocument')
      })

      await check('Pipeline convertAsync (bytes) matches convertFileAsync', async () => {
        const { readFileSync } = await import('node:fs')
        const res = await pipe.convertAsync({ name: 'doc.pdf', data: readFileSync(pdf) })
        assert.equal(res.content, (await pipe.convertFileAsync(pdf)).content)
      })

      await check('Pipeline streamFileMarkdown reproduces the buffered Markdown', async () => {
        let streamed = ''
        for await (const chunk of pipe.streamFileMarkdown(pdf)) {
          streamed += chunk
        }
        assert.equal(streamed, pipe.convertFile(pdf).content)
        assert.ok(streamed.length > 0)
      })

      await check('Pipeline streamFileMarkdown rejects referenced image mode', async () => {
        await assert.rejects(
          pipe.streamFileMarkdown(pdf, { imageMode: 'referenced' }).next(),
          /placeholder.*embedded|referenced/,
        )
      })

      await check('overlapping Pipeline async calls both resolve', async () => {
        const [a, b] = await Promise.all([pipe.convertFileAsync(pdf), pipe.convertFileAsync(pdf)])
        assert.equal(a.content, b.content)
      })
    } else {
      console.log('  --  PDF fixture not found; skipping warm-pipeline checks')
    }
  }

  console.log(`\n${passed} checks passed`)
}

main().catch(() => {
  process.exitCode = 1
})

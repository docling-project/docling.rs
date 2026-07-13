// Minimal smoke test exercising every part of the binding: sync + async
// convert, in-memory bytes, JSON output, the reusable class, streaming, and the
// format helpers. Run with `node test/smoke.mjs` (or `bun test/smoke.mjs`).
//
// Exits non-zero on the first failed assertion, so it doubles as a CI check.

import assert from 'node:assert/strict'
import {
  checkDependencies,
  convert,
  convertFile,
  convertFileAsync,
  DocumentConverter,
  formatFromName,
  Pipeline,
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

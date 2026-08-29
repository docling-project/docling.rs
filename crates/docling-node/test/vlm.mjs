// #290: the remote-VLM pipeline through the Node bindings, end-to-end against
// a local OpenAI-compatible stub — no models, no pdfium, no network beyond
// loopback (an image input is sent to the endpoint as-is, so nothing else is
// needed). Also pins the option-validation surface: explicit pipeline
// selection is required, unknown pipelines fail, and the warm Pipeline class
// refuses the VLM pipeline outright.
//
// Run with `node test/vlm.mjs` (part of `npm test`).

import assert from 'node:assert/strict'
import { createServer } from 'node:http'
import { convert, convertAsync, DocumentConverter, Pipeline } from '../index.js'

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

// A minimal chat-completions stub answering every request with well-formed
// DocLang (the granite-docling contract). Captures requests for assertions.
const requests = []
const server = createServer((req, res) => {
  let body = ''
  req.on('data', (d) => (body += d))
  req.on('end', () => {
    requests.push({ url: req.url, auth: req.headers.authorization, body: JSON.parse(body) })
    res.setHeader('content-type', 'application/json')
    res.end(
      JSON.stringify({
        choices: [
          { message: { content: '<heading level="2">Sec</heading>\n<text>Para from VLM.</text>' } },
        ],
      }),
    )
  })
})
await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve))
const endpoint = `http://127.0.0.1:${server.address().port}/v1`

// Any bytes work: an image input is posted to the endpoint verbatim.
const input = { name: 'page.png', data: Buffer.from('not-a-real-png') }

try {
  // Endpoint-hitting checks must be async: the stub server shares this
  // process's event loop, and a *sync* convert would block it — the request
  // could never be accepted (that's also the honest guidance for users:
  // prefer the async API with pipeline: 'vlm').
  await check('vlm pipeline converts through the endpoint', async () => {
    const res = await convertAsync(input, {
      pipeline: 'vlm',
      vlmEndpoint: endpoint,
      vlmModel: 'granite-docling',
      vlmApiKey: 'sekret',
      vlmPrompt: 'Custom prompt.',
      vlmMaxTokens: 4096,
    })
    assert.equal(res.status, 'success')
    assert.match(res.content, /## Sec/)
    assert.match(res.content, /Para from VLM\./)
    const r = requests.at(-1)
    assert.equal(r.url, '/v1/chat/completions', 'endpoint suffix appended')
    assert.equal(r.auth, 'Bearer sekret', 'api key forwarded')
    assert.equal(r.body.model, 'granite-docling')
    assert.equal(r.body.max_tokens, 4096)
    assert.match(JSON.stringify(r.body.messages), /Custom prompt\./)
  })

  await check('vlm pipeline works async and renders JSON', async () => {
    const res = await convertAsync(input, {
      pipeline: 'vlm',
      vlmEndpoint: endpoint,
      vlmModel: 'granite-docling',
      to: 'json',
    })
    const doc = JSON.parse(res.content)
    assert.equal(doc.schema_name, 'DoclingDocument')
    assert.match(res.content, /Para from VLM\./)
  })

  await check('DocumentConverter class carries the vlm config', async () => {
    const conv = new DocumentConverter({
      pipeline: 'vlm',
      vlmEndpoint: endpoint,
      vlmModel: 'granite-docling',
    })
    const res = await conv.convertAsync(input)
    assert.match(res.content, /## Sec/)
  })

  // Config-validation negatives use a non-ML input: the JS dependency guard
  // (which runs first) only fires for PDF/image, and the native validation
  // must reject these regardless of format.
  const mdInput = { name: 'x.md', data: Buffer.from('# t') }

  await check('vlm options without pipeline: "vlm" are an error, not dead weight', () => {
    assert.throws(
      () => convert(mdInput, { vlmEndpoint: endpoint }),
      /pipeline.*is not "vlm"/,
    )
  })

  await check('unknown pipeline id is an error', () => {
    assert.throws(() => convert(mdInput, { pipeline: 'quantum' }), /not standard\|vlm/)
  })

  await check('vlm without endpoint/model names the missing piece', () => {
    delete process.env.DOCLING_RS_VLM_ENDPOINT
    assert.throws(() => convert(input, { pipeline: 'vlm' }), /no endpoint/)
  })

  await check('warm Pipeline refuses the vlm pipeline', () => {
    assert.throws(
      () => new Pipeline({ pipeline: 'vlm' }),
      /warm ONNX models/,
    )
  })

  await check('streaming refuses the vlm pipeline', () => {
    const conv = new DocumentConverter({
      pipeline: 'vlm',
      vlmEndpoint: endpoint,
      vlmModel: 'granite-docling',
    })
    // An image path: the (pipeline-aware) deps guard needs nothing for
    // image+vlm, so the native streaming rejection is what surfaces.
    assert.throws(
      () => conv.convertFileStreaming('x.png', () => {}),
      /no streaming path/,
    )
  })
} finally {
  server.close()
}

console.log(`vlm: ${passed} checks passed`)

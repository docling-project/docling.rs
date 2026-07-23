// postinstall for the `docling.rs-cuda` npm package.
//
// The CUDA build of the addon is far too large to ship inside the npm tarball
// (the .node with the CUDA ONNX Runtime statically linked plus the two
// provider shared libraries total hundreds of MB, past npm's practical
// limits — the same reason onnxruntime-node fetches its CUDA binaries at
// install time). Instead this package is a few KB of JS, and this script
// downloads the binaries from the repo's GitHub release for this exact
// package version (tag `npm-cuda-v<version>`) into the package directory,
// where the napi loader (native.js) picks the .node up as a "local file" —
// the same resolution the prebuilt platform packages use.
//
// Integrity: the release carries a manifest.json with the sha256 of every
// asset; each download is verified against it before being moved into place.
// Idempotent: files already present with the right hash are not re-downloaded
// (re-running `npm install` / `npm ci` is a no-op once populated).
//
// Air-gapped / mirrored installs: set DOCLING_RS_NPM_CUDA_URL to a base URL
// (or a local directory path) that serves the same asset names.
'use strict'

const fs = require('fs')
const path = require('path')
const zlib = require('zlib')
const crypto = require('crypto')

const pkg = require('./package.json')
const DEST = __dirname
const TAG = `npm-cuda-v${pkg.version}`
const BASE =
  process.env.DOCLING_RS_NPM_CUDA_URL ||
  `https://github.com/docling-project/docling.rs/releases/download/${TAG}`

// Asset names on the release (gzipped) → final on-disk names next to this
// script. The .node name must match what native.js probes for on linux-x64.
const ASSETS = {
  'docling-rs.linux-x64-gnu.node.gz': 'docling-rs.linux-x64-gnu.node',
  'libonnxruntime_providers_shared.so.gz': 'libonnxruntime_providers_shared.so',
  'libonnxruntime_providers_cuda.so.gz': 'libonnxruntime_providers_cuda.so',
}

function fail(msg) {
  console.error(`docling.rs-cuda postinstall: ${msg}`)
  console.error(
    `  release: ${BASE}\n` +
      '  Set DOCLING_RS_NPM_CUDA_URL to a mirror (base URL or local directory) to override.',
  )
  process.exit(1)
}

// GET with redirect-following (GitHub release assets redirect to storage).
// A local-directory BASE short-circuits to a filesystem read.
function fetchBuffer(name) {
  const src = `${BASE.replace(/\/+$/, '')}/${name}`
  if (!/^https?:\/\//.test(BASE)) {
    return Promise.resolve(fs.readFileSync(path.join(BASE, name)))
  }
  const get = (url, redirects) =>
    new Promise((resolve, reject) => {
      if (redirects > 5) return reject(new Error(`too many redirects for ${name}`))
      require('https').get(url, { headers: { 'user-agent': 'docling.rs-cuda-postinstall' } }, (res) => {
        if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
          res.resume()
          return resolve(get(new URL(res.headers.location, url).toString(), redirects + 1))
        }
        if (res.statusCode !== 200) {
          res.resume()
          return reject(new Error(`HTTP ${res.statusCode} for ${url}`))
        }
        const chunks = []
        res.on('data', (c) => chunks.push(c))
        res.on('end', () => resolve(Buffer.concat(chunks)))
        res.on('error', reject)
      }).on('error', reject)
    })
  return get(src, 0)
}

const sha256 = (buf) => crypto.createHash('sha256').update(buf).digest('hex')

async function main() {
  if (process.platform !== 'linux' || process.arch !== 'x64') {
    // package.json os/cpu should have blocked this install already; double
    // check so a forced install fails with words instead of a broken addon.
    fail(`CUDA binaries are Linux x64 only (this is ${process.platform}-${process.arch})`)
  }

  let manifest
  try {
    manifest = JSON.parse((await fetchBuffer('manifest.json')).toString('utf8'))
  } catch (e) {
    fail(`cannot fetch manifest.json: ${e.message}`)
  }

  for (const [asset, out] of Object.entries(ASSETS)) {
    const want = manifest[out]
    if (!want) fail(`manifest.json has no sha256 for ${out}`)
    const dest = path.join(DEST, out)
    if (fs.existsSync(dest) && sha256(fs.readFileSync(dest)) === want) {
      console.log(`docling.rs-cuda: ${out} already present, skipping`)
      continue
    }
    process.stdout.write(`docling.rs-cuda: downloading ${asset} ... `)
    let raw
    try {
      raw = zlib.gunzipSync(await fetchBuffer(asset))
    } catch (e) {
      console.log('failed')
      fail(`${asset}: ${e.message}`)
    }
    console.log(`${(raw.length / 1048576).toFixed(0)} MB`)
    if (sha256(raw) !== want) fail(`${out}: sha256 mismatch — corrupted download or tampered mirror`)
    // Write via a temp name + rename so a killed install never leaves a
    // half-written .node that native.js would then try to dlopen.
    const tmp = dest + '.tmp'
    fs.writeFileSync(tmp, raw)
    fs.renameSync(tmp, dest)
  }
  console.log('docling.rs-cuda: ready (CUDA 12 + cuDNN 9 required at runtime; DOCLING_RS_EP overrides)')
}

main().catch((e) => fail(e.stack || String(e)))

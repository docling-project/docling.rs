// Assemble the publishable `docling.rs-cuda` npm package directory.
//
// The CUDA package is the SAME JavaScript surface as the main `docling.rs`
// package — index.js / deps.js / the napi-generated native.js loader and the
// .d.ts files are copied verbatim from a completed `npm run build:cuda` — plus
// the postinstall downloader. No binaries go into the tarball: postinstall.js
// fetches the CUDA .node + ONNX Runtime provider libraries from the GitHub
// release matching the package version (see postinstall.js).
//
// Usage (from crates/docling-node, after `npm run build:cuda`):
//   node cuda/make-package.mjs [out-dir]        # default: cuda/pkg
// The version is taken from the main package.json — run `npm version X` first
// (the publish workflow does) so both packages agree.
import { cpSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const nodeDir = dirname(dirname(fileURLToPath(import.meta.url)))
const out = process.argv[2] || join(nodeDir, 'cuda', 'pkg')
const main = JSON.parse(readFileSync(join(nodeDir, 'package.json'), 'utf8'))

rmSync(out, { recursive: true, force: true })
mkdirSync(out, { recursive: true })

for (const f of ['index.js', 'index.d.ts', 'deps.js', 'native.js', 'native.d.ts']) {
  cpSync(join(nodeDir, f), join(out, f))
}
cpSync(join(nodeDir, 'cuda', 'postinstall.js'), join(out, 'postinstall.js'))
cpSync(join(nodeDir, 'cuda', 'README.md'), join(out, 'README.md'))

writeFileSync(
  join(out, 'package.json'),
  JSON.stringify(
    {
      name: 'docling.rs-cuda',
      version: main.version,
      description:
        'CUDA build of docling.rs for Node.js / Bun (Linux x64). Same API as the docling.rs package; ' +
        'binaries are downloaded from the matching GitHub release at install time.',
      keywords: [...main.keywords, 'cuda', 'gpu'],
      license: main.license,
      repository: { ...main.repository, directory: 'crates/docling-node/cuda' },
      homepage: main.homepage,
      main: 'index.js',
      types: 'index.d.ts',
      os: ['linux'],
      cpu: ['x64'],
      engines: main.engines,
      scripts: { postinstall: 'node postinstall.js' },
    },
    null,
    2,
  ) + '\n',
)
console.log(`assembled docling.rs-cuda@${main.version} in ${out}`)

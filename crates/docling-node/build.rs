// napi-build wires up the N-API symbol resolution and, on Windows, the
// delay-load shim so the addon links against the host `node.exe`/`bun`.
fn main() {
    napi_build::setup();
}

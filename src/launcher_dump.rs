#[test]
fn dump_launcher_for_smoke_test() {
    let root = std::path::Path::new("/tmp/opencode/crun-test/container");
    let store = std::path::Path::new("/tmp/opencode/crun-test/store");
    let out = std::path::Path::new("/tmp/opencode/crun-test/launcher");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    let script = crate::crun::launcher("/usr/bin/hello", root, store);
    std::fs::write(out, script).unwrap();
}

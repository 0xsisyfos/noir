use std::path::PathBuf;

const PKG_NAME: Option<&'static str> = option_env!("CARGO_PKG_NAME");

fn lib_path() -> PathBuf {
    // Is there any way to automatically generate this?

    let dynamic_lib_output_name = format!("lib{}.so", PKG_NAME.unwrap());

    let mut path_to_output_dynamic_lib = path_to_target();
    path_to_output_dynamic_lib.push(dynamic_lib_output_name);

    path_to_output_dynamic_lib
}

fn path_to_target() -> PathBuf {
    const MANIFEST_DIR: Option<&'static str> = option_env!("CARGO_MANIFEST_DIR");
    let mut path_to_target = PathBuf::from(MANIFEST_DIR.unwrap());
    // Pop off a component to get to workspace root
    path_to_target.pop();
    // Pop off a component to get to root directory
    path_to_target.pop();

    path_to_target.push("target");
    path_to_target.push("debug");
    path_to_target
}

// Convert Rust library into a dynamic library, so we can test
fn make_helpers() {
    static ONCE: std::sync::Once = ::std::sync::Once::new();
    ONCE.call_once(|| {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let mut cmd = ::std::process::Command::new(cargo);
        cmd.arg("build");

        assert!(cmd
            .status()
            .expect("could not compile the test helpers!")
            .success());
    });
}
#[cfg(test)]
mod tests {

    use super::{lib_path, make_helpers};
    use noir_nd::make_extern_call;

    #[test]
    fn test_calling_dynamic_lib() {
        make_helpers();

        let name = String::from("func_name");
        let inputs = vec![[0u8; 32]; 1];
        let mut outputs = vec![[0u8; 32]; 2];

        make_extern_call(lib_path(), name, &inputs, &mut outputs);
        for o in outputs {
            println!("{}", hex::encode(&o))
        }
    }
}

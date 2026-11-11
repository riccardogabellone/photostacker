//! The executable Cargo builds must carry the name the packages and the desktop entry use.

use photostacker_ui::identity::BINARY_NAME;

#[test]
fn the_executable_is_named_as_identity_says() {
    let path = std::path::Path::new(env!(concat!("CARGO_BIN_EXE_", "Photostacker")));
    let expected = format!("{BINARY_NAME}{}", std::env::consts::EXE_SUFFIX);
    assert_eq!(path.file_name().unwrap(), expected.as_str());
}

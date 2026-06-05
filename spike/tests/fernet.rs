use spike::decrypt_fernet;

#[test]
fn decrypts_python_fernet_token_with_same_key() {
    let key = std::fs::read_to_string("tests/fixtures/fernet_key.txt").unwrap();
    let token = std::fs::read_to_string("tests/fixtures/fernet_token.txt").unwrap();
    let plaintext = decrypt_fernet(key.trim(), token.trim()).expect("decrypt should succeed");
    assert_eq!(plaintext, b"s3cr3t-password");
}

use hysteria_extras::obfs::{
    Obfuscator, SALAMANDER_SALT_LEN, SalamanderError, SalamanderObfuscator,
};
use rand::RngCore;

#[test]
fn salamander_roundtrip_works() {
    let obfs = SalamanderObfuscator::new("average_password").unwrap();
    let mut input = vec![0_u8; 1200];
    let mut encoded = vec![0_u8; 2048];
    let mut decoded = vec![0_u8; 2048];

    for _ in 0..128 {
        let mut rng = rand::rng();
        rng.fill_bytes(&mut input);

        let encoded_len = obfs.obfuscate(&input, &mut encoded);
        assert_eq!(encoded_len, input.len() + SALAMANDER_SALT_LEN);

        let decoded_len = obfs.deobfuscate(&encoded[..encoded_len], &mut decoded);
        assert_eq!(decoded_len, input.len());
        assert_eq!(&decoded[..decoded_len], input.as_slice());
    }
}

#[test]
fn salamander_rejects_short_psk() {
    let err = SalamanderObfuscator::new("abc").unwrap_err();
    assert_eq!(err, SalamanderError::PskTooShort);
}

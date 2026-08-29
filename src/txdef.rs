//! TXDEF encryption algorithm (ported from github.com/topxeq/tkc / xxDoMagic).
//!
//! A simple byte-level stream cipher with key-derived random padding.
//! Output format: uppercase hex of [random pad][ciphertext], no header prefix.
//! All arithmetic uses wrapping u8 to match the original Go implementation.
//!
//! Default key when code is empty: "topxeq".

const TXDEF_HEADER: &[u8] = b"//TXDEF#";
const DEFAULT_CODE: &str = "topxeq";

fn sum_bytes(b: &[u8]) -> u8 {
    b.iter().fold(0u8, |acc, &v| acc.wrapping_add(v))
}

/// Encrypt `data` with `code` (empty code uses the default "topxeq").
/// Returns the text form: uppercase hex of (pad + ciphertext). No header prefix.
pub fn encrypt(data: &[u8], code: &str) -> String {
    let code = if code.is_empty() { DEFAULT_CODE } else { code };
    let code_bytes = code.as_bytes();
    let code_len = code_bytes.len();
    let sum_t = sum_bytes(code_bytes) as usize;
    let add_len_t = (sum_t % 5) + 2;
    let enc_index = sum_t % add_len_t;

    // Generate random padding.
    let mut pad = vec![0u8; add_len_t];
    fill_random(&mut pad);

    // Encrypt.
    let mut buf = vec![0u8; data.len()];
    for i in 0..data.len() {
        buf[i] = data[i]
            .wrapping_add(code_bytes[i % code_len])
            .wrapping_add((i + 1) as u8)
            .wrapping_add(pad[enc_index]);
    }

    // Combine pad + ciphertext, uppercase hex encode. No header prefix.
    let mut payload = pad;
    payload.append(&mut buf);
    hex_encode_upper(&payload)
}

/// Decrypt a TXDEF text string (raw hex, `//TXDEF#<hex>`, `740404<hex>`,
/// or hex of a binary-headed payload — the binary header is also stripped).
/// Returns None if decryption fails. Empty code uses the default "topxeq".
pub fn decrypt(src: &str, code: &str) -> Option<Vec<u8>> {
    if src.is_empty() {
        return None;
    }
    // String-level prefix stripping: 740404 (legacy marker) first, then //TXDEF#.
    let s = src.strip_prefix("740404").unwrap_or(src);
    let s = s.strip_prefix("//TXDEF#").unwrap_or(s);
    let mut payload = hex_decode(s)?;

    // Binary-level header stripping: handle hex-encoded payloads that carry a binary
    // //TXDEF# header (e.g. char.exe encryptData -addHead output). Without this, the
    // header bytes would be treated as random salt, producing silent garbage.
    if payload.starts_with(TXDEF_HEADER) {
        payload.drain(0..TXDEF_HEADER.len());
    }

    let code = if code.is_empty() { DEFAULT_CODE } else { code };
    let code_bytes = code.as_bytes();
    let code_len = code_bytes.len();
    let sum_t = sum_bytes(code_bytes) as usize;
    let add_len_t = (sum_t % 5) + 2;
    if payload.len() < add_len_t {
        return None;
    }
    let enc_index = sum_t % add_len_t;
    let data_len = payload.len() - add_len_t;
    let mut buf = vec![0u8; data_len];
    for i in 0..data_len {
        buf[i] = payload[add_len_t + i]
            .wrapping_sub(code_bytes[i % code_len])
            .wrapping_sub((i + 1) as u8)
            .wrapping_sub(payload[enc_index]);
    }
    Some(buf)
}

/// Check whether a string starts with the TXDEF header.
#[allow(dead_code)]
pub fn is_encrypted(s: &str) -> bool {
    s.starts_with("//TXDEF#")
}

/// Convenience: encrypt a UTF-8 string.
pub fn encrypt_str(s: &str, code: &str) -> String {
    encrypt(s.as_bytes(), code)
}

/// Convenience: decrypt to a UTF-8 string.
pub fn decrypt_str(s: &str, code: &str) -> Option<String> {
    decrypt(s, code).and_then(|b| String::from_utf8(b).ok())
}

// ---- minimal hex encode/decode (no external dependency) ----

fn hex_encode_upper(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for &b in data {
        out.push(hex_digit(b >> 4));
        out.push(hex_digit(b & 0xf));
    }
    out
}

fn hex_digit(n: u8) -> char {
    if n < 10 {
        (b'0' + n) as char
    } else {
        (b'A' + (n - 10)) as char
    }
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    if bytes.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_val(bytes[i])?;
        let lo = hex_val(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Some(out)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Fill a buffer with random bytes (using a simple LCG seeded from time; not crypto-grade,
/// which is fine since the padding is re-read from ciphertext on decrypt).
fn fill_random(buf: &mut [u8]) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x1234_5678);
    for b in buf.iter_mut() {
        // xorshift64
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        *b = (seed & 0xff) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_basic() {
        let plain = "hello world";
        let enc = encrypt_str(plain, "");
        // No header prefix — just uppercase hex.
        assert!(!enc.starts_with("//TXDEF#"));
        let dec = decrypt_str(&enc, "").unwrap();
        assert_eq!(dec, plain);
    }

    #[test]
    fn roundtrip_unicode() {
        let plain = "密码测试 🔐 日本語";
        let enc = encrypt_str(plain, "");
        let dec = decrypt_str(&enc, "").unwrap();
        assert_eq!(dec, plain);
    }

    #[test]
    fn roundtrip_empty_string() {
        let plain = "";
        let enc = encrypt_str(plain, "");
        // Empty plaintext encrypts to just the header (pad only, no data).
        let dec = decrypt_str(&enc, "").unwrap_or_default();
        assert_eq!(dec, plain);
    }

    #[test]
    fn roundtrip_with_custom_code() {
        let plain = "secret password 123";
        let enc = encrypt_str(plain, "mykey");
        let dec = decrypt_str(&enc, "mykey").unwrap();
        assert_eq!(dec, plain);
    }

    #[test]
    fn wrong_code_fails_or_garbage() {
        let plain = "test data";
        let enc = encrypt_str(plain, "key1");
        // Decrypt with wrong key should either fail or produce garbage (not the original).
        let dec = decrypt_str(&enc, "key2");
        if let Some(d) = dec {
            assert_ne!(d, plain);
        }
    }

    #[test]
    fn plaintext_not_encrypted() {
        // is_encrypted checks for old TXDEF header; new format has no header.
        assert!(!is_encrypted("hello"));
        assert!(!is_encrypted(""));
    }

    #[test]
    fn different_encryptions_differ() {
        // Same plaintext + key should produce different ciphertexts (random padding).
        let plain = "same text";
        let e1 = encrypt_str(plain, "");
        let e2 = encrypt_str(plain, "");
        assert_ne!(e1, e2); // random padding makes them differ
        // But both decrypt to the same value.
        assert_eq!(decrypt_str(&e1, "").unwrap(), plain);
        assert_eq!(decrypt_str(&e2, "").unwrap(), plain);
    }

    #[test]
    fn hex_roundtrip() {
        let data = b"\x00\x01\x02\xff\xfe\xfdhello";
        let enc = hex_encode_upper(data);
        let dec = hex_decode(&enc).unwrap();
        assert_eq!(dec, data);
    }

    // ---- Official test vectors (from char.exe, verify decrypt direction) ----

    #[test]
    fn decrypt_official_text_vectors() {
        // Default key "topxeq"
        assert_eq!(
            decrypt_str("A2CAB6738C959E8F595181877C6B79585319FBCC1EE2F62A00BE23FBD528DFC428C9C7", "").unwrap(),
            "Hello, TXDEF! 你好，世界。"
        );
        // Custom key "char"
        assert_eq!(
            decrypt_str("AA0ED72E15E600F7C40711FF0B05C0FC1E0D1B0CD008171AD4102113281DD019321326CE341A1DD23017362FE81E2F21FADEF4EF02F5FCF70AFD04FF", "char").unwrap(),
            "The quick brown fox jumps over the lazy dog. 0123456789"
        );
        // //TXDEF# string prefix still works
        assert_eq!(
            decrypt_str("//TXDEF#A2CAB6738C959E8F595181877C6B79585319FBCC1EE2F62A00BE23FBD528DFC428C9C7", "topxeq").unwrap(),
            "Hello, TXDEF! 你好，世界。"
        );
    }

    #[test]
    fn decrypt_strips_binary_level_header() {
        // Simulates -addHead output: encrypt payload, prepend binary //TXDEF# header, hex encode.
        let plain = "binary header stripping test 二进制头剥离";
        let payload_hex = encrypt(plain.as_bytes(), "hk1");
        let payload = hex_decode(&payload_hex).unwrap();
        let mut with_head = TXDEF_HEADER.to_vec();
        with_head.extend_from_slice(&payload);
        let head_hex = hex_encode_upper(&with_head);

        // Fixed version: correctly strips binary header
        assert_eq!(decrypt(&head_hex, "hk1").unwrap(), plain.as_bytes());
        // String prefix + binary header simultaneously
        assert_eq!(decrypt(&format!("//TXDEF#{}", head_hex), "hk1").unwrap(), plain.as_bytes());
    }

    #[test]
    fn decrypt_official_binary_vector() {
        // Official char.exe vector: plaintext = bytes 0x00-0xFF, key abc123, with binary header
        let hex = "2F2F54584445462384ABDF666CC8CBCE9EA1A4D4D7DAAAADB0E0E3E6B6B9BCECEFF2C2C5C8F8FBFECED1D404070ADADDE0101316E6E9EC1C1F22F2F5F8282B2EFE010434373A0A0D1040434616191C4C4F52222528585B5E2E313464676A3A3D4070737646494C7C7F82525558888B8E5E616494979A6A6D70A0A3A676797CACAFB2828588B8BBBE8E9194C4C7CA9A9DA0D0D3D6A6A9ACDCDFE2B2B5B8E8EBEEBEC1C4F4F7FACACDD0000306D6D9DC0C0F12E2E5E8181B1EEEF1F424272AFAFD0030333606090C3C3F42121518484B4E1E212454575A2A2D3060636636393C6C6F72424548787B7E4E515484878A5A5D6090939666696C9C9FA2727578A8ABAE7E8184B4B7BA8A8D90C0C3C696";
        let plain: Vec<u8> = (0u8..=255).collect();
        assert_eq!(decrypt(hex, "abc123").unwrap(), plain);
    }

    #[test]
    fn decrypt_accepts_740404_prefix() {
        // Legacy TXDEM-era marker prefix
        let plain = "legacy marker test";
        let c = encrypt_str(plain, "lk1");
        assert_eq!(decrypt_str(&format!("740404{}", c), "lk1").unwrap(), plain);
    }

    #[test]
    fn decrypt_old_form_data_unchanged() {
        // Key compatibility test: headerless bare hex (xxssh's only output form) decrypts identically
        let plain = "previous xxssh data 旧数据";
        let c = encrypt_str(plain, "ok1");
        assert_eq!(decrypt_str(&c, "ok1").unwrap(), plain);
        // Empty input still returns None
        assert!(decrypt("", "k").is_none());
    }
}

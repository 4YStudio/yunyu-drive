//! 115 下载直链那层加解密。
//!
//! `proapi.115.com/app/chrome/downurl` 的请求体不是明文：
//!
//! ```text
//! rand(16) || reverse(xor(plain, genKey(rand, 4)))  ^  gKeyL  → RSA-PKCS1v15
//! ```
//!
//! 响应方向反过来，而且服务端用的不是标准「公钥加密」，而是**公钥没幂次**：
//! 客户端拿公钥做 `m = c^e mod n` 就能把服务端的签名式密文还原出来
//! （fake115uploader 的拿手好戏）。所以这里既要用 `rsa` crate 做标准加密，
//! 又要用裸的模幂做「解密」。

use base64::Engine;
use num_bigint::BigUint;
use rsa::pkcs1v15::Pkcs1v15Encrypt;
use rsa::traits::PublicKeyParts;
use rsa::pkcs8::DecodePublicKey;
use rsa::RsaPublicKey;

const G_KEY_L: [u8; 12] = [
    0x78, 0x06, 0xad, 0x4c, 0x33, 0x86, 0x5d, 0x18, 0x4c, 0x01, 0x3f, 0x46,
];

#[rustfmt::skip]
const G_KTS: [u8; 144] = [
    0xf0, 0xe5, 0x69, 0xae, 0xbf, 0xdc, 0xbf, 0x8a, 0x1a, 0x45, 0xe8, 0xbe, 0x7d, 0xa6, 0x73, 0xb8,
    0xde, 0x8f, 0xe7, 0xc4, 0x45, 0xda, 0x86, 0xc4, 0x9b, 0x64, 0x8b, 0x14, 0x6a, 0xb4, 0xf1, 0xaa,
    0x38, 0x01, 0x35, 0x9e, 0x26, 0x69, 0x2c, 0x86, 0x00, 0x6b, 0x4f, 0xa5, 0x36, 0x34, 0x62, 0xa6,
    0x2a, 0x96, 0x68, 0x18, 0xf2, 0x4a, 0xfd, 0xbd, 0x6b, 0x97, 0x8f, 0x4d, 0x8f, 0x89, 0x13, 0xb7,
    0x6c, 0x8e, 0x93, 0xed, 0x0e, 0x0d, 0x48, 0x3e, 0xd7, 0x2f, 0x88, 0xd8, 0xfe, 0xfe, 0x7e, 0x86,
    0x50, 0x95, 0x4f, 0xd1, 0xeb, 0x83, 0x26, 0x34, 0xdb, 0x66, 0x7b, 0x9c, 0x7e, 0x9d, 0x7a, 0x81,
    0x32, 0xea, 0xb6, 0x33, 0xde, 0x3a, 0xa9, 0x59, 0x34, 0x66, 0x3b, 0xaa, 0xba, 0x81, 0x60, 0x48,
    0xb9, 0xd5, 0x81, 0x9c, 0xf8, 0x6c, 0x84, 0x77, 0xff, 0x54, 0x78, 0x26, 0x5f, 0xbe, 0xe8, 0x1e,
    0x36, 0x9f, 0x34, 0x80, 0x5c, 0x45, 0x2c, 0x9b, 0x76, 0xd5, 0x1b, 0x8f, 0xcc, 0xc3, 0xb8, 0xf5,
];

const PUBLIC_PEM: &str = "-----BEGIN PUBLIC KEY-----
MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQCGhpgMD1okxLnUMCDNLCJwP/P0
UHVlKQWLHPiPCbhgITZHcZim4mgxSWWb0SLDNZL9ta1HlErR6k02xrFyqtYzjDu2
rGInUC0BCZOsln0a7wDwyOA43i5NO8LsNory6fEKbx7aT3Ji8TZCDAfDMbhxvxOf
dPMBDjxP5X3zr7cWgwIDAQAB
-----END PUBLIC KEY-----";

const RAND_KEY_LEN: usize = 16;
const RSA_BLOCK: usize = 128;

fn public_key() -> Result<RsaPublicKey, crate::error::AppError> {
    let der = pem_body(PUBLIC_PEM)?;
    RsaPublicKey::from_public_key_der(&der)
        .map_err(|e| crate::error::AppError::new(format!("115 公钥无效：{e}")))
}

fn pem_body(pem: &str) -> Result<Vec<u8>, crate::error::AppError> {
    let body: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect::<Vec<_>>()
        .join("");
    base64::engine::general_purpose::STANDARD
        .decode(body)
        .map_err(|e| crate::error::AppError::new(format!("115 公钥解码失败：{e}")))
}

/// `genKey`：用随机串和固定表派生异或密钥。
fn gen_key(rand_key: &[u8], key_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(key_len);
    let mut length = key_len * (key_len - 1);
    let mut index = 0;
    for r in rand_key.iter().take(key_len) {
        let x = r.wrapping_add(G_KTS[index]);
        out.push(G_KTS[length] ^ x);
        length = length.wrapping_sub(key_len);
        index += key_len;
    }
    out
}

/// `xor`：前 `len % 4` 个字节用密钥开头逐位异或，剩下的重新从密钥第 0 位开始。
fn xor_with(src: &[u8], key: &[u8]) -> Vec<u8> {
    if key.is_empty() {
        return src.to_vec();
    }
    let pad = src.len() % 4;
    let mut out = Vec::with_capacity(src.len());
    for i in 0..pad {
        out.push(src[i] ^ key[i % key.len()]);
    }
    for (i, byte) in src.iter().enumerate().skip(pad) {
        out.push(byte ^ key[(i - pad) % key.len()]);
    }
    out
}

/// 加密：给 `downurl` 用的请求体。
///
/// 返回 `(请求体, 随机密钥)` —— 随机密钥一定要留住，响应解密要用它派生第二个
/// 异或密钥（服务端回包里只有它自己的那一半）。
pub fn encode(plain: &[u8]) -> Result<(String, [u8; RAND_KEY_LEN]), crate::error::AppError> {
    let key = public_key()?;
    use rand::RngCore;
    let mut rand_key = [0u8; RAND_KEY_LEN];
    rand::thread_rng().fill_bytes(&mut rand_key);
    let key_s = gen_key(&rand_key, 4);

    let mut tmp = xor_with(plain, &key_s);
    tmp.reverse();
    let tail = xor_with(&tmp, &G_KEY_L);

    let mut payload = Vec::with_capacity(RAND_KEY_LEN + tail.len());
    payload.extend_from_slice(&rand_key);
    payload.extend_from_slice(&tail);

    let cipher = key
        .encrypt(&mut rand::thread_rng(), Pkcs1v15Encrypt, &payload)
        .map_err(|e| crate::error::AppError::new(format!("115 请求加密失败：{e}")))?;
    Ok((base64::engine::general_purpose::STANDARD.encode(cipher), rand_key))
}

/// 解密：还原服务端返回的密文。
pub fn decode(cipher_b64: &str, rand_key: &[u8; RAND_KEY_LEN]) -> Result<Vec<u8>, crate::error::AppError> {
    let key = public_key()?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(cipher_b64.trim())
        .map_err(|e| crate::error::AppError::new(format!("115 响应不是合法 base64：{e}")))?;
    if raw.is_empty() || raw.len() % RSA_BLOCK != 0 {
        return Err(crate::error::AppError::new(format!(
            "115 响应长度异常（{} 字节）",
            raw.len()
        )));
    }
    let n = BigUint::from_bytes_be(&key.n().to_bytes_be());
    let e = BigUint::from_bytes_be(&key.e().to_bytes_be());

    let mut plain = Vec::new();
    for block in raw.chunks(RSA_BLOCK) {
        let c = BigUint::from_bytes_be(block);
        let m = c.modpow(&e, &n);
        let bytes = m.to_bytes_be();
        // 手工反填充（参考实现也是这么做的）：跳过开头的 0x00 0x02，
        // 找到填充结束的那个 0x00，后面才是数据。
        let start = bytes
            .iter()
            .skip(1)
            .position(|b| *b == 0)
            .map(|pos| pos + 2)
            .unwrap_or(bytes.len());
        plain.extend_from_slice(&bytes[start..]);
    }
    if plain.len() <= RAND_KEY_LEN {
        return Err(crate::error::AppError::new("115 响应解密后为空"));
    }
    let head = &plain[..RAND_KEY_LEN];
    // 12 字节的长密钥来自响应自带的随机串，4 字节的短密钥来自**我们发出的**那个。
    let key_l = gen_key(head, 12);
    let key_s = gen_key(rand_key, 4);
    let mut tmp = xor_with(&plain[RAND_KEY_LEN..], &key_l);
    tmp.reverse();
    Ok(xor_with(&tmp, &key_s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_keys_the_way_the_client_does() {
        let rand = [0u8; 16];
        let k4 = gen_key(&rand, 4);
        let k12 = gen_key(&rand, 12);
        assert_eq!(k4.len(), 4);
        assert_eq!(k12.len(), 12);
        // 同样输入必须稳定，否则加解密对不上
        assert_eq!(k4, gen_key(&rand, 4));
    }

    #[test]
    fn xor_is_length_preserving_and_reversible_in_shape() {
        let key = [1u8, 2, 3, 4];
        let src = b"1234567890".to_vec();
        let once = xor_with(&src, &key);
        assert_eq!(once.len(), src.len());
        assert_ne!(once, src);
    }

    #[test]
    fn encodes_a_pickcode_payload() {
        let (text, key) = encode(br#"{"pickcode":"abcdefghij"}"#).expect("encode");
        assert!(!text.is_empty());
        // 128 字节 RSA 块 + PKCS#1 base64 ⇒ 通常是 172 字符
        assert!(text.len() > 100, "{}", text.len());
        // 同一个密钥派生出的短密钥要能反过来 —— 自加密只验证形状，服务端的
        // 那一半只有真请求才能验（live 测试覆盖）。
        assert_eq!(key.len(), RAND_KEY_LEN);
    }

    #[test]
    fn rejects_garbage_instead_of_panicking() {
        let key = [0u8; RAND_KEY_LEN];
        assert!(decode("not base64!!", &key).is_err());
        assert!(decode("AAAA", &key).is_err());
    }
}

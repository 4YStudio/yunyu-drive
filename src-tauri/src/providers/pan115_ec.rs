//! 115 上传接口的链路加密（对应 115driver 的 `pkg/crypto/ec115`）。
//!
//! `uplb.115.com/4.0/initupload.php` 不是普通的 JSON 接口：请求体先用
//! P-224 的 ECDH 共享密钥做 ASE-CBC 加密，响应则是 LZ4 压缩后再 CBC 加密回来，
//! 连"我是谁"都藏在 query 的 `k_ec` 里。这一层不破，上传根本发不出去。
//!
//! 三件事：
//! 1. 每次上传现生成一对 P-224 密钥，与服务端固定公钥算出共享密钥，
//!    `key = secret[0..16]`、`iv = secret[12..28]`（注意两者重叠，不是切片拼接）。
//! 2. `k_ec` 是把客户端公钥（29 字节压缩点）用两个随机字节异或打散后拼上
//!    时间戳和 CRC32，再 base64 —— 纯混淆，不是加密。
//! 3. 响应解密：CBC 解 → 前 2 字节是小端长度 → 后面是 LZ4 block。

use aes::cipher::{block_padding::{NoPadding, Pkcs7}, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use p224::elliptic_curve::sec1::ToEncodedPoint;
use p224::elliptic_curve::{ecdh::diffie_hellman, sec1::FromEncodedPoint};
use p224::{AffinePoint, EncodedPoint, SecretKey};
use rand::RngCore;

/// 服务端固定的 P-224 公钥，`x(28) || y(28)`（无 0x04 前缀）。
const REMOTE_PUB: [u8; 56] = [
    0x57, 0xA2, 0x92, 0x57, 0xCD, 0x23, 0x20, 0xE5, 0xD6, 0xD1, 0x43, 0x32, 0x2F, 0xA4, 0xBB, 0x8A,
    0x3C, 0xF9, 0xD3, 0xCC, 0x62, 0x3E, 0xF5, 0xED, 0xAC, 0x62, 0xB7, 0x67, 0x8A, 0x89, 0xC9, 0x1A,
    0x83, 0xBA, 0x80, 0x0D, 0x61, 0x29, 0xF5, 0x22, 0xD0, 0x34, 0xC8, 0x95, 0xDD, 0x24, 0x65, 0x24,
    0x3A, 0xDD, 0xC2, 0x50, 0x95, 0x3B, 0xEE, 0xBA,
];

const CRC_SALT: &[u8] = b"^j>WD3Kr?J2gLFjD4W2y@";
/// 解压缓冲。服务端响应都很小（几百字节），8 KiB 和参考实现一致。
const LZ4_BUF: usize = 0x2000;

type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;
type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

pub struct Cipher {
    key: [u8; 16],
    iv: [u8; 16],
    /// 客户端公钥，**带长度前缀**：`[0x1d, tag, X(28)]` 共 30 字节。
    /// 参考实现是 `append([]byte{29, 0x03|0x02}, X...)`，直接塞 29 字节
    /// 的 SEC1 压缩点会让服务端错位解析，回包也就解不开了。
    pub_key: [u8; 30],
}

impl Cipher {
    pub fn new() -> Result<Self, String> {
        // P-224 的标量是 28 字节；随机 32 字节大概率超阶，所以裁到 28 字节
        // 并把最高位清零，保证落在 [1, 2^223) 这个必然合法的区间里。
        let secret = loop {
            let mut seed = [0u8; 28];
            rand::thread_rng().fill_bytes(&mut seed);
            seed[0] &= 0x7f;
            if let Ok(secret) = SecretKey::from_slice(&seed) {
                break secret;
            }
        };
        let public = secret.public_key();

        let mut point = [0u8; 57];
        point[0] = 0x04;
        point[1..].copy_from_slice(&REMOTE_PUB);
        let encoded = EncodedPoint::from_bytes(point).map_err(|e| format!("服务端公钥无效：{e}"))?;
        let remote = AffinePoint::from_encoded_point(&encoded)
            .into_option()
            .ok_or_else(|| "服务端公钥不在曲线上".to_string())?;
        let shared = diffie_hellman(secret.to_nonzero_scalar(), remote);
        let secret_bytes = shared.raw_secret_bytes();

        let compressed = public.to_encoded_point(true);
        let compressed = compressed.as_bytes();
        if compressed.len() != 29 {
            return Err(format!("公钥编码长度异常：{}", compressed.len()));
        }

        let mut key = [0u8; 16];
        let mut iv = [0u8; 16];
        key.copy_from_slice(&secret_bytes[..16]);
        iv.copy_from_slice(&secret_bytes[12..28]);
        let mut pub_key = [0u8; 30];
        pub_key[0] = 29;
        pub_key[1..].copy_from_slice(compressed);
        Ok(Self { key, iv, pub_key })
    }

    pub fn encrypt(&self, plain: &[u8]) -> Vec<u8> {
        Aes128CbcEnc::new(&self.key.into(), &self.iv.into()).encrypt_padded_vec_mut::<Pkcs7>(plain)
    }

    pub fn decrypt(&self, cipher_text: &[u8]) -> Result<Vec<u8>, String> {
        let usable = cipher_text.len() - cipher_text.len() % 16;
        if usable == 0 {
            return Err("响应为空".into());
        }
        // 服务端不补 PKCS7：解出来的明文自带长度前缀，末尾的填充字节要原样留着，
        // 交给 LZ4 按长度截断（参考实现也是这么干的）。
        let raw = Aes128CbcDec::new(&self.key.into(), &self.iv.into())
            .decrypt_padded_vec_mut::<NoPadding>(&cipher_text[..usable])
            .map_err(|e| format!("响应解密失败：{e}"))?;
        if raw.len() < 2 {
            return Err("响应太短".into());
        }
        // 前两字节是 LZ4 块的字节数（小端）。
        let len = usize::from(raw[0]) | (usize::from(raw[1]) << 8);
        let end = len
            .checked_add(2)
            .filter(|end| *end <= raw.len())
            .ok_or_else(|| format!("响应长度字段越界（{len} > {}）", raw.len() - 2))?;
        let mut out = vec![0u8; LZ4_BUF];
        let written = lz4_flex::block::decompress_into(&raw[2..end], &mut out)
            .map_err(|e| format!("响应解压失败：{e}"))?;
        out.truncate(written);
        Ok(out)
    }

    /// `k_ec`：混淆过的公钥 + 时间戳 + CRC32，base64。
    pub fn encode_token(&self, timestamp_ms: i64) -> String {
        let mut rng = rand::thread_rng();
        let mut one = [0u8; 1];
        let mut two = [0u8; 1];
        rng.fill_bytes(&mut one);
        rng.fill_bytes(&mut two);
        let (r1, r2) = (one[0], two[0]);

        let stamp = (timestamp_ms as u32).to_be_bytes();
        let mut out = Vec::with_capacity(48);
        // 前 15 字节用 r1 打散，中间夹 5 个校验字节。
        for byte in &self.pub_key[..15] {
            out.push(byte ^ r1);
        }
        out.push(r1);
        out.push(0x73 ^ r1);
        out.extend_from_slice(&[r1; 3]);
        // 时间戳按小端塞进 4 个字节里。
        for i in 0..4 {
            out.push(r1 ^ stamp[3 - i]);
        }
        // 后 15 字节用 r2 打散。
        for byte in &self.pub_key[15..] {
            out.push(byte ^ r2);
        }
        out.push(r2);
        out.push(0x01 ^ r2);
        out.extend_from_slice(&[r2; 3]);

        let mut hasher = crc32fast::Hasher::new();
        hasher.update(CRC_SALT);
        hasher.update(&out);
        let crc = hasher.finalize().to_be_bytes();
        for i in 0..4 {
            out.push(crc[3 - i]);
        }
        base64_encode(&out)
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_48_bytes_of_base64() {
        let cipher = Cipher::new().expect("cipher");
        let token = cipher.encode_token(1_700_000_000_000);
        use base64::Engine;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(token)
            .expect("base64");
        assert_eq!(raw.len(), 48, "token 结构固定 48 字节");
    }

    #[test]
    fn round_trip_survives_cbc_and_lz4() {
        let cipher = Cipher::new().expect("cipher");
        // 手工拼一个与线上同构的响应：长度前缀 + LZ4 块。
        let payload = br#"{"status":1,"state":true}"#;
        let mut framed = Vec::new();
        let compressed = lz4_flex::block::compress(payload);
        framed.extend_from_slice(&(compressed.len() as u16).to_le_bytes());
        framed.extend_from_slice(&compressed);
        let encrypted = cipher.encrypt(&framed);
        let back = cipher.decrypt(&encrypted).expect("decrypt");
        assert_eq!(back, payload);
    }

    #[test]
    fn two_ciphers_get_different_keys() {
        let a = Cipher::new().expect("a");
        let b = Cipher::new().expect("b");
        assert_ne!(a.pub_key, b.pub_key);
        let plain = b"hello";
        assert_ne!(a.encrypt(plain), b.encrypt(plain));
    }

    #[test]
    fn garbage_does_not_panic() {
        let cipher = Cipher::new().expect("cipher");
        assert!(cipher.decrypt(&[0u8; 16]).is_err());
        assert!(cipher.decrypt(&[]).is_err());
    }
}

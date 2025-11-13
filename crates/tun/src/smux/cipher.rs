use anyhow::{Result, anyhow};
pub trait Encryptor: Send + Sync + Unpin {
    fn encypt<InOut>(&mut self, buf: &mut InOut) -> Result<()>
    where
        InOut: AsRef<[u8]> + AsMut<[u8]> + for<'in_out> Extend<&'in_out u8>;
}

pub trait Decyptor: Send + Sync + Unpin {
    fn decrypt<InOut>(&mut self, buf: &mut InOut) -> Result<()>
    where
        InOut: AsRef<[u8]> + AsMut<[u8]> + for<'in_out> Extend<&'in_out u8>;
}

pub trait NonceAdance: Send + Sync + Unpin {
    fn advance(&mut self) -> Result<Vec<u8>>;
}

pub trait Cipher<N>: Send + Sync + Unpin {
    type Enc;
    type Dec;

    fn encyptor(&self, key: &[u8], nonce: N) -> Result<Self::Enc>;

    fn decyptor(&self, key: &[u8], nonce: N) -> Result<Self::Dec>;
}

mod aead {
    use std::collections::HashMap;

    use super::*;
    use lazy_static::lazy_static;
    use ring::aead::{self, Aad, Algorithm, LessSafeKey, Nonce};
    lazy_static! {
        static ref AEAD_LIST: HashMap<&'static str, &'static Algorithm> = {
            let mut m = HashMap::new();
            m.insert("chacha20-poly1305", &aead::CHACHA20_POLY1305);
            m.insert("chacha20-ietf-poly1305", &aead::CHACHA20_POLY1305);
            m.insert("aes-256-gcm", &aead::AES_256_GCM);
            m.insert("aes-128-gcm", &aead::AES_128_GCM);
            m
        };
    }

    pub struct AeadEncryptor<N> {
        enc: LessSafeKey,
        nonce: N,
    }

    impl<N> Encryptor for AeadEncryptor<N>
    where
        N: NonceAdance,
    {
        #[inline]
        fn encypt<InOut>(&mut self, buf: &mut InOut) -> Result<()>
        where
            InOut: AsRef<[u8]> + AsMut<[u8]> + for<'in_out> Extend<&'in_out u8>,
        {
            let n = self.nonce.advance()?;
            let nonce = Nonce::try_assume_unique_for_key(&n).map_err(|e| anyhow!(e.to_string()))?;
            self.enc
                .seal_in_place_append_tag(nonce, Aad::empty(), buf)
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            Ok(())
        }
    }

    impl<N> Decyptor for AeadDecryptor<N>
    where
        N: NonceAdance,
    {
        /// Decrypts the given buffer in place.
        ///
        #[inline]
        fn decrypt<InOut>(&mut self, buf: &mut InOut) -> Result<()>
        where
            InOut: AsRef<[u8]> + AsMut<[u8]> + for<'in_out> Extend<&'in_out u8>,
        {
            let n = self.nonce.advance()?;
            let nonce = Nonce::try_assume_unique_for_key(&n).map_err(|e| anyhow!(e.to_string()))?;
            self.enc
                .open_in_place(nonce, Aad::empty(), buf.as_mut())
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            Ok(())
        }
    }

    pub struct AeadDecryptor<N> {
        enc: LessSafeKey,
        nonce: N,
    }

    pub struct AeadCipher {
        alogrithm: &'static Algorithm,
    }

    impl AeadCipher {
        pub fn new(cipher: &str) -> Result<Self> {
            let alogrithm = AEAD_LIST.get(cipher).ok_or(anyhow!("unknown cipher"))?;
            Ok(Self { alogrithm })
        }
    }

    impl<N> Cipher<N> for AeadCipher {
        type Enc = AeadEncryptor<N>;

        type Dec = AeadDecryptor<N>;

        #[inline]
        fn encyptor(&self, key: &[u8], nonce: N) -> Result<Self::Enc> {
            let ubound_key = ring::aead::UnboundKey::new(self.alogrithm, key)
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            Ok(AeadEncryptor {
                enc: LessSafeKey::new(ubound_key),
                nonce,
            })
        }

        #[inline]
        fn decyptor(&self, key: &[u8], nonce: N) -> Result<Self::Dec> {
            let ubound_key = ring::aead::UnboundKey::new(self.alogrithm, key)
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            Ok(AeadDecryptor {
                enc: LessSafeKey::new(ubound_key),
                nonce,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode() {
        let pkg = [
            0x2, 0x0, 0x0, 0x0, 0x45, 0x0, 0x0, 0x70, 0xb, 0xed, 0x0, 0x0, 0x40, 0x11, 0x13, 0xe9,
            0x90, 0xca, 0x9, 0xcc, 0xc0, 0x10, 0x0, 0x1, 0x49, 0xf3, 0xc9, 0xd4, 0x0, 0x5c, 0x86,
            0xf, 0x91, 0x64, 0x37, 0x4f, 0x52, 0x71, 0x36, 0xb7, 0x5f, 0xcd, 0x61, 0xd5, 0x5e,
            0x8d, 0x6c, 0xf6, 0xd2, 0xe0, 0xfd, 0x59, 0x31, 0xb, 0x14, 0x84, 0x48, 0xeb, 0xf, 0xf6,
            0x72, 0x42, 0x4a, 0x39, 0xac, 0x2d, 0xcb, 0x6a, 0x43, 0x24, 0xa2, 0xe6, 0x14, 0x3d,
            0x7d, 0x89, 0x1b, 0x9c, 0xd3, 0x98, 0x3d, 0xe6, 0x21, 0xac, 0xad, 0x8b, 0x83, 0x11,
            0x9a, 0x59, 0x63, 0x81, 0xd4, 0x0, 0xfa, 0x13, 0x25, 0xc5, 0xc9, 0xfd, 0xe4, 0xb4,
            0xc8, 0x39, 0x71, 0xe2, 0x77, 0x2c, 0xde, 0xdb, 0xf4, 0xf5, 0xb2, 0xf4, 0xb9, 0x47,
        ];
        let ciper = aead::AeadCipher::new("aes-128-gcm").unwrap();
    }
}

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

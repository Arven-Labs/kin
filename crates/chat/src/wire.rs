use crate::Error;

/// O que trafega entre dois peers. O handshake é in-band: quem tem o menor Peer ID convida.
///
/// `Hello` (ambos, ao conectar) → o de maior Peer ID responde `KeyPackage` → o de menor cria a
/// conversa e manda `Welcome` → daí em diante só `Mls`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Envelope {
    /// Com o token do convite (P18), se o link o tinha; sem ele, payload vazio.
    Hello(Option<[u8; 16]>),
    KeyPackage(Vec<u8>),
    Welcome(Vec<u8>),
    Mls(Vec<u8>),
}

impl Envelope {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let (tag, payload): (u8, &[u8]) = match self {
            // STUB (testes primeiro): ainda não leva o token.
            Self::Hello(_) => (0, &[]),
            Self::KeyPackage(b) => (1, b),
            Self::Welcome(b) => (2, b),
            Self::Mls(b) => (3, b),
        };
        let mut out = vec![tag];
        out.extend_from_slice(payload);
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let (&tag, payload) = bytes.split_first().ok_or(Error::Malformed)?;
        Ok(match tag {
            0 if payload.is_empty() => Self::Hello(None),
            1 => Self::KeyPackage(payload.to_vec()),
            2 => Self::Welcome(payload.to_vec()),
            3 => Self::Mls(payload.to_vec()),
            _ => return Err(Error::Malformed),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: [u8; 16] = [7; 16];

    /// P18: o `Hello` de quem aceitou um link v3 leva o token; sem link, vai vazio como antes.
    #[test]
    fn a_hello_with_a_token_roundtrips_and_one_without_stays_empty() {
        let with = Envelope::Hello(Some(TOKEN)).encode();
        assert_eq!(with.len(), 1 + 16, "tag + 16 bytes de token");
        assert_eq!(
            Envelope::decode(&with).unwrap(),
            Envelope::Hello(Some(TOKEN))
        );

        let without = Envelope::Hello(None).encode();
        assert_eq!(without, vec![0], "só a tag: igual ao formato antigo");
        assert_eq!(Envelope::decode(&without).unwrap(), Envelope::Hello(None));
    }

    /// Entrada externa: payload de `Hello` que não tem 0 nem 16 bytes é recusado, sem pânico.
    #[test]
    fn a_hello_payload_of_any_other_size_is_malformed() {
        for len in [1, 5, 15, 17, 64] {
            let mut bytes = vec![0];
            bytes.extend(std::iter::repeat_n(9, len));
            assert!(
                matches!(Envelope::decode(&bytes), Err(Error::Malformed)),
                "payload de {len} bytes"
            );
        }
    }
}

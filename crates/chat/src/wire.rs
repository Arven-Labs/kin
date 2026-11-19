use crate::Error;

/// O que trafega entre dois peers. O handshake é in-band: quem tem o menor Peer ID convida.
///
/// `Hello` (ambos, ao conectar) → o de maior Peer ID responde `KeyPackage` → o de menor cria a
/// conversa e manda `Welcome` → daí em diante só `Mls`.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Envelope {
    /// Com o token do convite (P18), se o link o tinha, e com o certificado do device (P18 PR 3, ADR 0039):
    /// é por ele que quem recebe conhece a identidade **antes** de mandar o `KeyPackage`. Sem certificado
    /// vai o formato antigo (tag 0, só o token), que ainda é lido.
    Hello(Option<[u8; 16]>, Option<Vec<u8>>),
    KeyPackage(Vec<u8>),
    Welcome(Vec<u8>),
    Mls(Vec<u8>),
}

/// `Debug` à mão: o token do `Hello` é um segredo até ser usado e não pode sair em log nem em mensagem
/// de teste (auditoria, baixo 3). O resto sai como o `derive` faria.
impl std::fmt::Debug for Envelope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Hello(token, certificate) => f
                .debug_tuple("Hello")
                .field(&token.map(|_| "<oculto>"))
                .field(&certificate.as_ref().map(Vec::len))
                .finish(),
            Self::KeyPackage(bytes) => f.debug_tuple("KeyPackage").field(bytes).finish(),
            Self::Welcome(bytes) => f.debug_tuple("Welcome").field(bytes).finish(),
            Self::Mls(bytes) => f.debug_tuple("Mls").field(bytes).finish(),
        }
    }
}

impl Envelope {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let (tag, payload): (u8, &[u8]) = match self {
            Self::Hello(None, None) => (0, &[]),
            Self::Hello(Some(token), None) => (0, token),
            // Tag 4: `tem token u8 (0 ou 1) | [token; 16] | certificado (o resto)`.
            Self::Hello(token, Some(certificate)) => {
                let mut out = vec![4, u8::from(token.is_some())];
                out.extend(token.iter().flatten());
                out.extend_from_slice(certificate);
                return out;
            }
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
            // Payload vazio (sem token) ou exatamente 16 bytes (o token); qualquer outro tamanho é
            // entrada malformada.
            0 if payload.is_empty() => Self::Hello(None, None),
            0 => Self::Hello(
                Some(<[u8; 16]>::try_from(payload).map_err(|_| Error::Malformed)?),
                None,
            ),
            // Com certificado: a flag é 0 ou 1 e o certificado não pode ser vazio. O conteúdo do
            // certificado só é validado por quem o usa (`on_hello`).
            4 => {
                let (&flag, rest) = payload.split_first().ok_or(Error::Malformed)?;
                let (token, certificate) = match flag {
                    0 => (None, rest),
                    1 => {
                        let (token, certificate) =
                            rest.split_at_checked(16).ok_or(Error::Malformed)?;
                        (
                            Some(<[u8; 16]>::try_from(token).map_err(|_| Error::Malformed)?),
                            certificate,
                        )
                    }
                    _ => return Err(Error::Malformed),
                };
                if certificate.is_empty() {
                    return Err(Error::Malformed);
                }
                Self::Hello(token, Some(certificate.to_vec()))
            }
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
        let with = Envelope::Hello(Some(TOKEN), None).encode();
        assert_eq!(with.len(), 1 + 16, "tag + 16 bytes de token");
        assert_eq!(
            Envelope::decode(&with).unwrap(),
            Envelope::Hello(Some(TOKEN), None)
        );

        let without = Envelope::Hello(None, None).encode();
        assert_eq!(without, vec![0], "só a tag: igual ao formato antigo");
        assert_eq!(
            Envelope::decode(&without).unwrap(),
            Envelope::Hello(None, None)
        );
    }

    /// P18 PR 3: o `Hello` com certificado (tag 4) volta igual, com e sem token.
    #[test]
    fn a_hello_with_a_certificate_roundtrips_with_and_without_a_token() {
        let cert = vec![1, 2, 3, 4];
        for token in [Some(TOKEN), None] {
            let hello = Envelope::Hello(token, Some(cert.clone()));
            let bytes = hello.encode();
            assert_eq!(bytes[0], 4);
            assert_eq!(Envelope::decode(&bytes).unwrap(), hello);
        }
    }

    /// Entrada externa: `Hello` de tag 4 truncado ou com flag inválida é recusado, sem pânico.
    #[test]
    fn a_malformed_hello_with_a_certificate_is_refused() {
        // Sem flag; sem certificado; token curto; token completo sem certificado; flag inválida.
        let mut full_token_only = vec![4, 1];
        full_token_only.extend_from_slice(&TOKEN);
        let cases = [
            vec![4],
            vec![4, 0],
            vec![4, 1, 7, 7],
            full_token_only,
            vec![4, 2, 9, 9],
        ];
        for bytes in cases {
            assert!(
                matches!(Envelope::decode(&bytes), Err(Error::Malformed)),
                "{bytes:?}"
            );
        }
    }

    /// Auditoria (baixo 3): o token do `Hello` é um segredo e o `Debug` não o mostra.
    #[test]
    fn the_debug_output_of_a_hello_hides_the_token() {
        let shown = format!("{:?}", Envelope::Hello(Some([0xAB; 16]), None));
        assert!(shown.contains("<oculto>"), "{shown}");
        assert!(!shown.contains("171"), "o token vazou: {shown}");
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

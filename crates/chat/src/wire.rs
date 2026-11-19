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
    ///
    /// O terceiro campo (P16, tag 5) é "tenho conversa com você": `None` nos formatos antigos (tags 0 e 4),
    /// que por isso **nunca** disparam o re-handshake; quem envia hoje sempre manda `Some`.
    Hello(Option<[u8; 16]>, Option<Vec<u8>>, Option<bool>),
    KeyPackage(Vec<u8>),
    Welcome(Vec<u8>),
    Mls(Vec<u8>),
}

/// `Debug` à mão: o token do `Hello` é um segredo até ser usado e não pode sair em log nem em mensagem
/// de teste (auditoria, baixo 3). O resto sai como o `derive` faria.
impl std::fmt::Debug for Envelope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Hello(token, certificate, has_conversation) => f
                .debug_tuple("Hello")
                .field(&token.map(|_| "<oculto>"))
                .field(&certificate.as_ref().map(Vec::len))
                .field(has_conversation)
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
            Self::Hello(None, None, None) => (0, &[]),
            Self::Hello(Some(token), None, None) => (0, token),
            // Tag 4 (formato do PR 3a, só lido por quem recebe de um build anterior): `tem token u8 (0 ou
            // 1) | [token; 16] | certificado (o resto)`.
            Self::Hello(token, Some(certificate), None) => {
                let mut out = vec![4, u8::from(token.is_some())];
                out.extend(token.iter().flatten());
                out.extend_from_slice(certificate);
                return out;
            }
            // Tag 5: `bits u8 | [token; 16] se bit 1 | certificado (o resto) se bit 4`. Bit 1 = tem token,
            // bit 2 = tenho conversa com você (P16), bit 4 = tem certificado.
            Self::Hello(token, certificate, Some(has_conversation)) => {
                let bits = u8::from(token.is_some())
                    | u8::from(*has_conversation) << 1
                    | u8::from(certificate.is_some()) << 2;
                let mut out = vec![5, bits];
                out.extend(token.iter().flatten());
                out.extend(certificate.iter().flatten());
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
            0 if payload.is_empty() => Self::Hello(None, None, None),
            0 => Self::Hello(
                Some(<[u8; 16]>::try_from(payload).map_err(|_| Error::Malformed)?),
                None,
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
                Self::Hello(token, Some(certificate.to_vec()), None)
            }
            // Tag 5: só os bits 1, 2 e 4 existem; o que o bit 1 e o bit 4 prometem tem de estar lá, e o
            // certificado prometido não pode ser vazio.
            5 => {
                let (&bits, rest) = payload.split_first().ok_or(Error::Malformed)?;
                if bits & !0b111 != 0 {
                    return Err(Error::Malformed);
                }
                let (token, rest) = if bits & 1 != 0 {
                    let (token, rest) = rest.split_at_checked(16).ok_or(Error::Malformed)?;
                    (
                        Some(<[u8; 16]>::try_from(token).map_err(|_| Error::Malformed)?),
                        rest,
                    )
                } else {
                    (None, rest)
                };
                let certificate = if bits & 4 != 0 {
                    if rest.is_empty() {
                        return Err(Error::Malformed);
                    }
                    Some(rest.to_vec())
                } else if rest.is_empty() {
                    None
                } else {
                    return Err(Error::Malformed);
                };
                Self::Hello(token, certificate, Some(bits & 2 != 0))
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
        let with = Envelope::Hello(Some(TOKEN), None, None).encode();
        assert_eq!(with.len(), 1 + 16, "tag + 16 bytes de token");
        assert_eq!(
            Envelope::decode(&with).unwrap(),
            Envelope::Hello(Some(TOKEN), None, None)
        );

        let without = Envelope::Hello(None, None, None).encode();
        assert_eq!(without, vec![0], "só a tag: igual ao formato antigo");
        assert_eq!(
            Envelope::decode(&without).unwrap(),
            Envelope::Hello(None, None, None)
        );
    }

    /// P16: o `Hello` da tag 5 volta igual em todas as combinações de token, certificado e "tenho conversa".
    #[test]
    fn a_hello_of_tag_5_roundtrips_in_every_combination() {
        let cert = vec![1, 2, 3, 4];
        for token in [Some(TOKEN), None] {
            for certificate in [Some(cert.clone()), None] {
                for has_conversation in [true, false] {
                    let hello = Envelope::Hello(token, certificate.clone(), Some(has_conversation));
                    let bytes = hello.encode();
                    assert_eq!(bytes[0], 5);
                    assert_eq!(Envelope::decode(&bytes).unwrap(), hello, "{bytes:?}");
                }
            }
        }
    }

    /// P16: o bit "tenho conversa" é só o bit 2: a tag 5 sem token nem certificado tem 2 bytes.
    #[test]
    fn the_conversation_bit_is_bit_two_of_tag_5() {
        assert_eq!(Envelope::Hello(None, None, Some(false)).encode(), vec![5, 0]);
        assert_eq!(Envelope::Hello(None, None, Some(true)).encode(), vec![5, 2]);
    }

    /// Entrada externa: tag 5 com bit desconhecido, truncada ou com sobra é recusada, sem pânico. O que o
    /// bit 1 (token) ou o bit 4 (certificado) promete tem de estar lá, e nada além disso.
    #[test]
    fn a_malformed_hello_of_tag_5_is_refused() {
        let mut token_only = vec![5, 1];
        token_only.extend_from_slice(&TOKEN);
        let mut token_and_stray_byte = token_only.clone();
        token_and_stray_byte.push(9);
        let cases = [
            vec![5],
            vec![5, 8],
            vec![5, 0xFF],
            vec![5, 1, 7, 7],
            vec![5, 4],
            vec![5, 0, 9],
            vec![5, 2, 9],
            token_and_stray_byte,
        ];
        for bytes in cases {
            assert!(
                matches!(Envelope::decode(&bytes), Err(Error::Malformed)),
                "{bytes:?}"
            );
        }
        assert!(Envelope::decode(&token_only).is_ok(), "só o token é válido");
    }

    /// P18 PR 3: o `Hello` com certificado (tag 4) volta igual, com e sem token.
    #[test]
    fn a_hello_with_a_certificate_roundtrips_with_and_without_a_token() {
        let cert = vec![1, 2, 3, 4];
        for token in [Some(TOKEN), None] {
            let hello = Envelope::Hello(token, Some(cert.clone()), None);
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
        let shown = format!("{:?}", Envelope::Hello(Some([0xAB; 16]), None, Some(true)));
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

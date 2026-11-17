//! `/devices` e `/revoke` no primário (P30 fatia 5, ADR 0034).
//!
//! O primário guarda cada certificado que emite em `issued/<peer-id>.cert` (um arquivo por device,
//! nunca reescrito). É o "livro de registro": `/devices` o lista e `/revoke` tira dele o certificado
//! que o `revoke_device` exige. O estado (ativo ou revogado) **não** mora aqui: vem do store de tombstones.

use std::path::Path;

use kin_identity::{DeviceCertificate, IdentityId, PeerId};

/// Pasta, dentro do `--data-dir` do primário, com os certificados emitidos.
pub const ISSUED_DIR: &str = "issued";

/// Começo comum a todos os Peer IDs Ed25519: não ajuda a distinguir devices.
const COMMON_PREFIX: &str = "12D3KooW";

/// Menor número de caracteres, **depois** do `12D3KooW`, aceito em `/revoke`.
pub const MIN_PREFIX_LEN: usize = 4;

/// Teto do tamanho de um arquivo de `issued/` (um certificado v4 tem poucos KiB).
const MAX_FILE_LEN: u64 = 64 * 1024;

/// O que `/revoke` deve fazer com a linha digitada.
#[derive(Debug, PartialEq, Eq)]
pub enum RevokeAction<'a> {
    /// Sem `confirmo`: mostrar qual device é e pedir a confirmação.
    Ask(&'a DeviceCertificate),
    /// Com `confirmo`: revogar de fato.
    Revoke(&'a DeviceCertificate),
}

/// Grava `certificate` em `<data_dir>/issued/<peer-id>.cert` (0600). Idempotente: se o arquivo do
/// mesmo device já existe, não o reescreve e devolve `Ok`.
pub fn record(data_dir: &Path, certificate: &DeviceCertificate) -> Result<(), String> {
    let dir = data_dir.join(ISSUED_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| format!("não deu para criar {ISSUED_DIR}/: {e}"))?;
    let path = dir.join(file_name(certificate));
    match crate::pair::write_new_atomic(&path, &certificate.to_bytes()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(format!("não deu para registrar o device emitido: {e}")),
    }
}

fn file_name(certificate: &DeviceCertificate) -> String {
    format!("{}.cert", certificate.network_peer_id())
}

/// Os certificados emitidos para `identity`, em ordem de Peer ID. Arquivo ilegível, corrompido, com
/// assinatura inválida ou de **outra identidade** é ignorado (a listagem nunca falha por causa dele).
pub fn list(data_dir: &Path, identity: &IdentityId) -> Vec<DeviceCertificate> {
    let Ok(entries) = std::fs::read_dir(data_dir.join(ISSUED_DIR)) else {
        return Vec::new();
    };
    let mut issued: Vec<DeviceCertificate> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            if !meta.is_file() || meta.len() > MAX_FILE_LEN {
                return None;
            }
            let certificate =
                DeviceCertificate::from_bytes(&std::fs::read(entry.path()).ok()?).ok()?;
            certificate.verify().ok()?;
            // Só da minha identidade, e só se o nome do arquivo é o do device (sem duplicata).
            let named_right = entry.file_name().to_str() == Some(file_name(&certificate).as_str());
            (certificate.identity() == identity && named_right).then_some(certificate)
        })
        .collect();
    issued.sort_by_key(|certificate| certificate.network_peer_id());
    issued
}

/// Resolve o que foi digitado em `/revoke ARGS`: `PREFIXO` ou `PREFIXO confirmo`. O prefixo é do Peer
/// ID (com ou sem o `12D3KooW` do começo) e precisa ser único entre `issued`; o device `own` (o do
/// próprio primário) nunca pode ser o alvo.
pub fn decide_revoke<'a>(
    args: &str,
    issued: &'a [DeviceCertificate],
    own: &PeerId,
) -> Result<RevokeAction<'a>, String> {
    let mut words: Vec<&str> = args.split_whitespace().collect();
    let confirmed = words.last() == Some(&"confirmo");
    if confirmed {
        words.pop();
    }
    let [typed] = words.as_slice() else {
        return Err("uso: /revoke PREFIXO_DO_PEER_ID [confirmo] (veja /devices)".into());
    };
    let tail = typed.strip_prefix(COMMON_PREFIX).unwrap_or(typed);
    if tail.chars().count() < MIN_PREFIX_LEN {
        return Err(format!(
            "prefixo curto demais: use pelo menos {MIN_PREFIX_LEN} caracteres depois de {COMMON_PREFIX}"
        ));
    }
    let wanted = format!("{COMMON_PREFIX}{tail}");
    let mut matches = issued.iter().filter(|certificate| {
        certificate
            .network_peer_id()
            .to_string()
            .starts_with(&wanted)
    });
    let (Some(target), None) = (matches.next(), matches.next()) else {
        let count = issued
            .iter()
            .filter(|c| c.network_peer_id().to_string().starts_with(&wanted))
            .count();
        return Err(if count == 0 {
            format!("nenhum device emitido começa por {typed} (veja /devices)")
        } else {
            format!("prefixo {typed} é ambíguo ({count} devices): digite mais caracteres")
        });
    };
    if &target.network_peer_id() == own {
        return Err("este é o próprio device do primário: não dá para revogá-lo aqui".into());
    }
    Ok(if confirmed {
        RevokeAction::Revoke(target)
    } else {
        RevokeAction::Ask(target)
    })
}

/// Uma linha de `/devices`: Peer ID, data de emissão (segundos Unix) e estado.
pub fn describe(certificate: &DeviceCertificate, revoked: bool, connected: bool) -> String {
    let state = if revoked { "revogado" } else { "ativo" };
    let link = if connected { ", conectado" } else { "" };
    format!(
        "{}  emitido em {}  {state}{link}",
        certificate.network_peer_id(),
        certificate.created_at
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_identity::{
        AuthorityIdentity, AuthorityKey, DeviceKey, IdentityProvider, StandaloneIdentity,
    };

    const NOW: u64 = 1_800_000_000;

    fn primary() -> AuthorityIdentity {
        let master = StandaloneIdentity::generate();
        let key = AuthorityKey::generate();
        let chain = master.certify_authority(&key.public_key(), 1).unwrap();
        AuthorityIdentity::new(key, chain).unwrap()
    }

    fn issue(primary: &AuthorityIdentity, at: u64) -> (DeviceCertificate, DeviceKey) {
        let device = DeviceKey::generate();
        let cert = primary
            .authorize_device(
                &DeviceKey::generate().public_key(),
                &device.public_key(),
                &device.nostr_public_key(),
                at,
            )
            .unwrap();
        (cert, device)
    }

    fn certs(primary: &AuthorityIdentity, n: usize) -> Vec<DeviceCertificate> {
        (0..n).map(|i| issue(primary, NOW + i as u64).0).collect()
    }

    // ---- record ----

    #[test]
    fn record_writes_the_certificate_in_issued_named_by_peer_id_with_private_permissions() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let (cert, _) = issue(&primary(), NOW);

        record(dir.path(), &cert).unwrap();

        let path = dir
            .path()
            .join(ISSUED_DIR)
            .join(format!("{}.cert", cert.network_peer_id()));
        assert_eq!(
            DeviceCertificate::from_bytes(&std::fs::read(&path).unwrap()).unwrap(),
            cert
        );
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn recording_the_same_device_twice_is_ok_and_keeps_the_first_file() {
        let dir = tempfile::tempdir().unwrap();
        let primary = primary();
        let (cert, _) = issue(&primary, NOW);
        let mut again = cert.clone();
        again.created_at = NOW + 99; // outro certificado do mesmo device

        record(dir.path(), &cert).unwrap();
        record(dir.path(), &again).unwrap();

        let files: Vec<_> = std::fs::read_dir(dir.path().join(ISSUED_DIR))
            .unwrap()
            .collect();
        assert_eq!(files.len(), 1, "sobrou temporário ou duplicado");
        let kept = std::fs::read(files[0].as_ref().unwrap().path()).unwrap();
        assert_eq!(DeviceCertificate::from_bytes(&kept).unwrap(), cert);
    }

    // ---- list ----

    #[test]
    fn list_returns_the_issued_certificates_in_peer_id_order() {
        let dir = tempfile::tempdir().unwrap();
        let primary = primary();
        let issued = certs(&primary, 3);
        for cert in &issued {
            record(dir.path(), cert).unwrap();
        }

        let listed = list(dir.path(), &primary.id());

        let mut expected = issued;
        expected.sort_by_key(|c| c.network_peer_id());
        assert_eq!(listed, expected);
    }

    #[test]
    fn list_without_the_folder_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list(dir.path(), &primary().id()).is_empty());
    }

    #[test]
    fn list_skips_garbage_forged_and_foreign_files_without_failing() {
        let dir = tempfile::tempdir().unwrap();
        let (mine, other) = (primary(), primary());
        let good = issue(&mine, NOW).0;
        record(dir.path(), &good).unwrap();
        record(dir.path(), &issue(&other, NOW).0).unwrap(); // de outra identidade
        let folder = dir.path().join(ISSUED_DIR);
        std::fs::write(folder.join("lixo.cert"), b"nao e certificado").unwrap();
        let mut forged = issue(&mine, NOW).0;
        forged.signature[0] ^= 1; // assinatura adulterada
        std::fs::write(
            folder.join(format!("{}.cert", forged.network_peer_id())),
            forged.to_bytes(),
        )
        .unwrap();

        assert_eq!(list(dir.path(), &mine.id()), vec![good]);
    }

    // ---- decide_revoke ----

    fn peer_str(cert: &DeviceCertificate) -> String {
        cert.network_peer_id().to_string()
    }

    #[test]
    fn revoke_without_confirmation_only_asks_and_with_it_revokes_the_same_device() {
        let issued = certs(&primary(), 2);
        let own = PeerId::random();
        let target = &issued[1];
        let prefix: String = peer_str(target).chars().take(20).collect();

        assert_eq!(
            decide_revoke(&prefix, &issued, &own).unwrap(),
            RevokeAction::Ask(target)
        );
        assert_eq!(
            decide_revoke(&format!("{prefix} confirmo"), &issued, &own).unwrap(),
            RevokeAction::Revoke(target)
        );
        // espaços sobrando e o prefixo sem o `12D3KooW` do começo também valem
        let short = &peer_str(target)["12D3KooW".len()..][..12];
        assert_eq!(
            decide_revoke(&format!("  {short}   confirmo "), &issued, &own).unwrap(),
            RevokeAction::Revoke(target)
        );
    }

    #[test]
    fn revoke_refuses_empty_too_short_unknown_and_ambiguous_prefixes() {
        let issued = certs(&primary(), 2);
        let own = PeerId::random();
        let full = peer_str(&issued[0]);

        for bad in [
            "",
            "confirmo",
            "12D3",                  // curto demais
            &full[..11],             // um a menos que o mínimo
            "12D3KooWnaoexisteaqui", // não existe
            "12D3KooW",              // comum a todos: ambíguo (e curto)
        ] {
            assert!(
                decide_revoke(bad, &issued, &own).is_err(),
                "aceitou {bad:?}"
            );
        }
    }

    #[test]
    fn revoke_refuses_an_ambiguous_prefix_that_is_long_enough() {
        let primary = primary();
        let a = issue(&primary, NOW).0;
        let mut b = issue(&primary, NOW + 1).0;
        // força o mesmo prefixo de 12 caracteres nos dois: troca a chave de rede de `b` não serve (o
        // Peer ID sai dela), então o teste usa dois certificados do MESMO device em `issued`.
        b.network_key = a.network_key.clone();
        let issued = vec![a.clone(), b];

        let err = decide_revoke(&peer_str(&a)[..20], &issued, &PeerId::random()).unwrap_err();
        assert!(err.contains("ambíguo"), "{err}");
    }

    #[test]
    fn revoke_refuses_the_primarys_own_device() {
        let issued = certs(&primary(), 1);
        let own = issued[0].network_peer_id();

        let err = decide_revoke(&peer_str(&issued[0])[..20], &issued, &own).unwrap_err();
        assert!(err.contains("próprio"), "{err}");
    }

    // ---- tombstone e descrição ----

    #[test]
    fn the_tombstone_built_from_the_registered_certificate_revokes_that_device_only() {
        let primary = primary();
        let dir = tempfile::tempdir().unwrap();
        let issued = certs(&primary, 2);
        for cert in &issued {
            record(dir.path(), cert).unwrap();
        }
        let listed = list(dir.path(), &primary.id());

        let revocation = primary.revoke_device(&listed[0], NOW).unwrap();

        revocation.verify().unwrap();
        assert!(revocation.revokes(&listed[0]));
        assert!(!revocation.revokes(&listed[1]));
    }

    #[test]
    fn describe_shows_peer_id_issue_time_and_state() {
        let (cert, _) = issue(&primary(), NOW);
        let peer = cert.network_peer_id().to_string();

        let active = describe(&cert, false, false);
        let revoked = describe(&cert, true, false);
        let connected = describe(&cert, false, true);

        for line in [&active, &revoked, &connected] {
            assert!(line.contains(&peer), "{line}");
            assert!(line.contains(&NOW.to_string()), "{line}");
        }
        assert!(active.contains("ativo") && !active.contains("revogado"));
        assert!(revoked.contains("revogado") && !revoked.contains("ativo"));
        assert!(connected.contains("conectado"));
        assert!(!active.contains("conectado"));
    }
}

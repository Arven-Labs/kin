use zeroize::Zeroizing;

/// Chave de 32 bytes dos bancos SQLite cifrados com SQLCipher (P13, ADR 0045).
///
/// Zera a memória ao sair de escopo e nunca aparece em `Debug`. Não implementa `Display`, `Clone` nem
/// `Serialize`: quem precisa dos bytes (só o ponto que aplica o `PRAGMA key`) chama [`Self::expose_secret`].
pub struct DbKey(Zeroizing<[u8; 32]>);

impl DbKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// Os bytes da chave. Não os guarde nem os registre em log.
    pub fn expose_secret(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for DbKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DbKey(<redigida>)")
    }
}

# ADR 0033 — Convite com expiração assinada (P30 fatia 4, P18)

- **Status**: **aceito e implementado** (Sessão 28, 2026-11-17). Área de segurança (assinatura e parsing de entrada externa, AGENTS.md §4): auditado pelo
  `security-auditor` (0 críticos, 3 importantes, 7 sugestões; tabela no fim).
- **Relacionados**: ADR 0011 (decisão 5, frescor), P18, P30, ADR 0027 (tombstone no `verify`).

## Contexto

O ADR 0011 tornou o frescor obrigatório: sem validade, o link de convite de um device revogado segue discável para quem nunca viu a tombstone. Decisão do dono
(2026-11-17): **convite com expiração primeiro**, a prova de frescor periódica do primário fica para depois. O `Invite` não tem assinatura própria (só o certificado
é assinado), então um `expires_at` solto poderia ser editado por quem tem o link.

## Decisão

1. **Link v2** (`kin://invite/`): `versão 2 | len16 certificado | n u8 | n × (len16 endereço) | expires_at u64 (BE) | len16 assinatura`. A assinatura é Ed25519 da
   **chave de rede do device** (a do certificado) sobre `"kin/invite/v2" | todos os bytes anteriores`; cobre versão, certificado, endereços e validade.
2. **Leitura** (`Invite::from_link`, com `from_link_at(link, now)` para teste): confere certificado, assinatura e, nesta ordem, **teto** (`expires_at > now + 7 dias` →
   `InvalidInvite`) e **vencimento** (`now >= expires_at` → `ExpiredInvite`; no instante exato já venceu). Link v1 é recusado (`InvalidInvite`).
3. **Emissão**: `Chat::invite_link(addrs, ttl)`; `DEFAULT_INVITE_TTL` = 24 h; `ttl` é limitado a 7 dias menos 5 minutos (folga para relógio do leitor).
4. **Só no link**: `DeviceList`/`Payload::DeviceList` seguem com o convite v1 sem validade; `Invite` não carrega `expires_at` depois de `from_link`, e
   `Chat::accept(&Invite)` **não** confere validade (o único chamador hoje, a CLI, lê o link e aceita na hora).
5. `Chat` guarda uma cópia do `DeviceKey` (`signer`) só para assinar links.

## Alternativas descartadas

- **`not_after` no certificado v4**: protege de verdade de device roubado, mas muda o formato do certificado e exige renovação periódica; fica para a "prova de frescor".
- **`expires_at` sem assinatura**: qualquer um edita; só cosmético.
- **Aceitar links v1 numa transição**: mantém o problema do P18 vivo.
- **Validade dentro do `Invite` e checada no `accept`**: mais estrito, mas mexeria na `DeviceList`; adiado até haver um segundo chamador de `accept`.

## Consequências e limites

- **Device roubado emite convites novos** com a própria chave: a expiração **não** o contém; quem contém é a tombstone (ADRs 0022 a 0029). Limite aceito.
- O link é *bearer* e multiuso dentro da janela (sem nonce, destinatário nem revogação): até 24 h (7 dias se o emissor for malicioso) para um device cuja revogação o leitor ainda não conhece.
- Quem guardar um `Invite` e chamar `accept` depois aceita um convite possivelmente vencido (documentado em `Chat::accept`).
- Relógios: leitor adiantado recusa link válido; atrasado aceita vencido por esse desvio. Sem tolerância de desvio na leitura.
- `now_secs()` devolve 0 se o relógio falhar; na leitura isso falha fechado, na emissão gera link já vencido (não tratado).
- Quebra de API: `Invite::to_link` agora recebe a chave e a validade; consumidores fora deste repo (p. ex. `warden/mobile`) não foram conferidos.
- Tamanho: o v2 soma 74 bytes; o link com certificado v4 (cadeia `KAUT`) não foi medido contra o limite do QR.

## Auditoria (`security-auditor`, somente leitura)

| # | Achado | Gravidade | Destino |
|---|---|---|---|
| 1 | `signer` copia o `DeviceKey` inteiro (inclui a chave Nostr) por um `Vec` sem zeroizar | importante | **aberto**, decisão do dono: guardar só a chave de rede |
| 2 | Teste não pegava a remoção do domínio, nem adulteração de endereços | importante | corrigido (3 testes novos) |
| 3 | `invite_link` aceitava `ttl` que gera link morto | importante | corrigido (limite e testes) |
| 4 | `accept` não vê a validade | sugestão | documentado em `Chat::accept` e acima |
| 5 | Replay e escopo do link | sugestão | registrado acima |
| 6 | Referência ao ADR 0033 inexistente | sugestão | este arquivo |
| 7 | Doc de `Invite` obsoleto | sugestão | corrigido |
| 8 | `to_link` não confere se a chave é a do certificado | sugestão | não feito: os testes 5 e 12 assinam com outra chave de propósito |
| 9 | CLI não mostra a validade | sugestão | corrigido ("válido por 24 h") |
| 10 | Tamanho do link v4 | sugestão | não medido |

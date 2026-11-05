# ADR 0002 — Outbox local, entrega ordenada e deduplicação por MessageId (Fase 3.2)

- **Status**: aceito (Sessão 6, 2026-11-05).
- **Relacionados**: D1 em `ARCHITECTURE.md`; ADR 0001 (P9); P13, P16, P19 e P20 em `PENDING.md`.

## Contexto

Até a Fase 2, enviar a um peer offline falhava (`SendFailed`): a mensagem só chegava se os dois estivessem
online ao mesmo tempo. D1 manda começar por uma outbox local (opção d), antes de qualquer mailbox de terceiros.
Três problemas apareceram ao desenhá-la:

1. **O que guardar.** Cifrar na hora de escrever prende o ciphertext a uma época MLS (só 3 passadas são
   guardadas) e consome gerações de chave à toa; guardar em claro deixa texto legível em repouso.
2. **Nova tentativa duplica.** Reenviar uma mensagem re-cifrada produz bytes diferentes; o MLS só reconhece
   repetição de bytes idênticos (`Duplicate`), então o receptor mostraria a mensagem duas vezes.
3. **Ordem.** Envios em paralelo seguem por substreams diferentes e a rede não garante qual chega primeiro.
   Com janela de 8 vimos `["dois","um","tres"]` em 1 de ~60 rodadas do teste de ponta a ponta. O `sent_at_ms`
   não resolve: 3 envios seguidos empatam no mesmo milissegundo.

## Decisão

1. **Texto em claro na fila, cifra ao enviar.** SQLite próprio do `chat` (`<banco>.outbox.db`, 0600), separado
   do banco do `crypto`. A exposição não é de uma classe nova: as chaves MLS já estão sem cifra em repouso (P13).
2. **Re-cifra a cada tentativa e deduplicação no receptor pelo `MessageId`** (chave: peer + id; 1024 ids por
   peer, os mais antigos são esquecidos). A mensagem só sai da fila com o ack de transporte.
3. **Um envio por vez** (`SEND_WINDOW = 1`): a próxima só sai depois do ack da anterior, então a ordem de
   chegada é a da fila. Custo: uma ida e volta por mensagem ao esvaziar a fila.
4. **Fila em ordem de escrita**; sai ao `PeerConnected` e quando a conversa fica pronta. O evento
   `ChatEvent::Queued` avisa que a mensagem está esperando (peer offline, janela ocupada ou envio falho).
   `SendFailed` fica só para mensagens de handshake.

## Alternativas descartadas

- **Guardar o ciphertext da 1ª tentativa e reenviar os mesmos bytes.** O MLS dedupicaria sozinho, mas o
  ciphertext envelhece e vira `TooOld` (ignorado em silêncio) depois de 3 épocas.
- **Janela de 8 sem prometer ordem.** Mais rápido para filas grandes, mas joga a ordenação para a interface,
  que precisaria de número de sequência no formato (P20).
- **Tabela no banco do `crypto` ou segunda conexão no mesmo arquivo.** Camada errada / risco de
  `database is locked`.

## Consequências

- Uma mensagem escrita com o peer offline não se perde nem aparece duplicada, inclusive se o remetente reinicia.
- Reentrega atrasada de uma mensagem cujo id já foi esquecido (depois de 1024) apareceria de novo.
- Esvaziar uma fila grande é lento (uma ida e volta por mensagem).
- Se o envio falha com a conexão ainda aberta, a mensagem só tenta de novo na próxima conexão (P19).
- Perda de estado do receptor (P16) continua silenciosa: o ack é de transporte, não de leitura.

## Em aberto

- **P19** reenvio periódico com espera crescente; **P20** número de sequência por remetente no formato
  da mensagem (v2), que permite subir a janela de volta e ordenar na interface.
- A janela deve continuar abaixo das 10 gerações fora de ordem que o MLS tolera.
- Mailbox de terceiros (3.1, 3.3, 3.4): sem recibo de entrega do destinatário o remetente não sabe se o
  depósito chegou; a outbox reenvia direto ao reconectar e a deduplicação cobre os dois caminhos.

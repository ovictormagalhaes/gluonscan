# Adding a protocol adapter

An adapter is a feature-gated crate `gluonscan-<protocol>` that implements `ProtocolAdapter`. The
rules below make it impossible for a new adapter to break the integrity contract.

```rust,ignore
#[async_trait]
impl ProtocolAdapter for MyProtocol {
    fn protocol(&self) -> Protocol { Protocol::MyProtocol }
    fn source(&self) -> Source { Source::Api }              // which backend
    fn capabilities(&self) -> &'static [Capability] { &[Capability::Positions] }
    fn supported_chains(&self) -> &'static [Chain] { &[Chain::Ethereum] }

    async fn read(&self, owner: &Wallet, chain: Chain, detail: Detail, cx: &Ctx)
        -> Result<Complete<Reading>, Error> { /* fetch + normalize */ }
}
```

## Rules

- **No I/O of your own.** Reach the network only through the injected `cx` ports (`http`, `rpc`).
  Never construct a client or hardcode an endpoint.
- **No state.** Anything needing prior state goes through an injected store, never a global.
- **Return the complete resource.** Map every meaningful field a resource provides; don't trim to
  "what one consumer needs."
- **Never fabricate.** Missing price → `AbsentPrice`; required-call failure → `Integrity`; 429 →
  `Transient`. Never a `0`.
- **`Decimal` + base units, never `f64`** in any value path. Prices are parsed from their literal,
  not through a float.
- **Bind only to supported chains** via `supported_chains()`.

## Ship with tests

Every adapter PR includes offline, contract-based tests (see
[Testing with contracts](contract-testing.md)) covering the happy path, the empty case, and the
failure shapes (429, malformed, missing price) — each asserting error/skip, never a zero.

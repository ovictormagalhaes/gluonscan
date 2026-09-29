# Testing with contracts

Adapters are tested entirely offline against **contracts** — recorded pairs of a request filter and
the response to reply with — using the mock transports in `gluonscan-testing`. Because there is no
network and no keys, these tests run in any CI, including on pull requests from forks.

```rust,ignore
use gluonscan_testing::{MockHttp, MockChainProvider, MockClock, Match};

// HTTP / GraphQL
let http = MockHttp::new()
    .on(Match::body_contains("userSupplies"), r#"{"data":{"userSupplies":[]}}"#);

// On-chain RPC
let rpc = MockChainProvider::new()
    .on(Match::all([Match::method("eth_call"), Match::body_contains("0xC364")]), reply);

let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));
```

## Filters

A `Match` filters the outgoing request: `PrimaryContains` / `method` (URL or RPC method),
`BodyContains`, `JsonEq { pointer }`, composed with `all` / `any_of`. A request that matches **no**
contract is an error, so tests fail loudly on unexpected calls.

## Saved contracts

Contracts can live in code (builder style, above) or as JSON files loaded with `load_contracts`,
with the recorded response inline or in a sibling file — so real provider payloads, including their
ugly failure shapes, are checked in and replayed.

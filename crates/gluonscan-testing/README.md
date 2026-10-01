<p align="center"><a href="https://github.com/ovictormagalhaes/gluonscan"><img src="https://raw.githubusercontent.com/ovictormagalhaes/gluonscan/main/assets/icon.png" width="72" alt="gluonscan" /></a></p>

# gluonscan-testing

Contract-based mock transports for gluonscan: replay recorded request/response contracts (HTTP/GraphQL, on-chain RPC; gRPC-ready) matched by filters.

Part of [**gluonscan**](https://crates.io/crates/gluonscan) — a stateless multi-chain *read + normalize* engine for DeFi portfolios. Most users depend on the `gluonscan` facade crate, which re-exports this one; depend on `gluonscan-testing` directly only to trim your build to a single slice.

## License

Dual-licensed under either [MIT](../../LICENSE-MIT) or [Apache-2.0](../../LICENSE-APACHE), at your option.

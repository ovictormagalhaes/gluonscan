# The data-integrity contract

People make financial decisions from these numbers, so correctness is enforced by the type system,
not by convention.

## Complete or error

Every read returns `Result<Complete<T>, Error>`. `Complete<T>` has **no public partial
constructor** — an adapter only produces one when every required source succeeded. Incomplete data
is therefore untypeable at the API boundary.

## Never a fabricated value

- A missing or unavailable price is `Error::AbsentPrice` — never coerced to `0` or `1`.
- A required source that fails is `Error::Integrity` — the read fails closed rather than return a
  partial answer.
- A transient failure (HTTP 429, timeout) is `Error::Transient` and reports `is_retryable() == true`.

```rust,ignore
pub enum Error {
    Transient { message: String, retry_after: Option<Duration> },
    Permanent { message: String },
    Integrity { message: String },
    AbsentPrice { asset: String },
    Unsupported { protocol: Protocol, capability: Capability, backend: Source },
    Provider(Box<dyn std::error::Error + Send + Sync>),
}
```

## Return completeness

An adapter returns **everything a fetched resource provides** — never a cherry-picked subset. The
detail level chooses *which* resources to fetch, not *which fields* of a fetched resource to keep.
(The subgraph is the one exception: it is query-shaped, so refinement happens in the query.)

## Provenance

Every value can carry its `Provenance`: which backend produced it, block height / timestamp, and a
staleness assessment — so trust travels with the number, and a fallback that lowers fidelity is
visible rather than silent.

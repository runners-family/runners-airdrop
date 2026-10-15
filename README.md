# runners-airdrop

The Solana program that distributes $RUNNERS: it holds the entire supply and lets every eligible wallet claim its
allocation once, until a deadline. After the deadline, anyone can burn what was not claimed.

There is no admin. Once a distribution is initialized, nothing can change its list of wallets, its total or its
deadline. The program can be upgraded until its upgrade authority is removed; the current authority is shown on-chain.

## Instructions

1. **Initialize** `(root, total, deadline)`, signed by the creator. It creates the distribution at the address
   `["airdrop", mint, creator]` and checks that:
   - the distribution's token account holds exactly `total`, and that is the mint's whole supply;
   - the mint has no mint authority and no freeze authority, and no extensions other than the transfer fee and
     metadata;
   - the deadline is in the future and at most 400 days away.

   It records the transfer fee in effect at that moment.
2. **Claim** `(amount, proof)`, signed by the claiming wallet, until the deadline.
   - **Proof:** the wallet proves its entitlement with a Merkle proof against the fixed root.
     - Leaf: `sha256("RUNNERS:airdrop:leaf:v1" ‖ distribution ‖ wallet ‖ amount)`.
     - Node: `sha256("RUNNERS:airdrop:node:v1" ‖ sorted pair)`.
   - **One claim per wallet:** a claim marker at `["claim", distribution, wallet]` makes each claim one-time.
   - **Destination:** the tokens go only to a token account the wallet owns. If the transfer fee rises above the one
     recorded at initialization, claims wait until it is back down.
3. **Burn unclaimed**, callable by anyone after the deadline: burns everything still in the distribution's account.

## Build

The deployed program is built in Solana's verifiable-build environment, so anyone can rebuild it byte for byte:

```bash
solana-verify build --base-image solanafoundation/solana-verifiable-build:4.3.0 --library-name runners_airdrop
```

`tests/fee_equivalence.rs` checks the program's fee arithmetic against Token-2022's (`cargo test --release`).

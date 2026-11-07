# NABLA Orders

[![verify](https://github.com/Nabla-Solana/nabla-orders/actions/workflows/verify.yml/badge.svg)](https://github.com/Nabla-Solana/nabla-orders/actions/workflows/verify.yml)

The Solana program behind [NABLA](https://nabladefi.com)'s automatic orders: limit buys, stop loss and take profit. A user's tokens stay in their own wallet under a spending approval until a fill. A fill can only spend the order's amount, only through Jupiter, only into the user's own account, and for at least the minimum the user signed.

| | |
| --- | --- |
| Program | `AAAtde5ZAg136H2RrNtMohS7k8AVprbGBBEqhT1S8uGA` |
| Upgrade authority and admin | Squads v4 vault `224s36sbZzri87Q6neo2GZwC4Aunz2wm1znM7MoEJZQm` |
| Squads multisig | `8UfPE1mk5WEfaeRxyrTaEvPWq7S3f1zg8WfCo17x3qnj` |
| IDL | On-chain through Program Metadata (seed `idl`) |

## Check the deployed program yourself

On every push and once a day, the `verify` workflow builds this source in solana-verify's container and compares it with the program on mainnet. The badge above shows the latest result.

On your own machine, with Docker and [solana-verify](https://github.com/otter-sec/solana-verifiable-build):

```sh
solana-verify build --library-name nabla_orders --base-image solanafoundation/solana-verifiable-build:4.3.0
solana-verify get-executable-hash target/deploy/nabla_orders.so
solana-verify get-program-hash -u https://api.mainnet-beta.solana.com AAAtde5ZAg136H2RrNtMohS7k8AVprbGBBEqhT1S8uGA
```

The two hashes are equal when the deployed program was built from this source.

## Layout

- `programs/nabla-orders`: the program (Anchor 1.2.1)
- `programs/mock-swap`: a stand-in for Jupiter used in tests, never deployed

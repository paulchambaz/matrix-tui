run *ARGS:
  cargo run {{ ARGS }}

watch *ARGS:
  cargo watch -x run {{ ARGS }}

build:
  cargo build --release

test:
  cargo test --all-targets -- --include-ignored

watch-test:
  cargo watch -x test

coverage:
  cargo tarpaulin

fmt:
  cargo fmt

lint:
  cargo clippy --all-targets -- -D warnings

man:
  scdoc < mx.1.scd > mx.1

install prefix=(env_var('HOME') + '/.local'): build man
  install -Dm755 target/release/mx {{ prefix }}/bin/mx
  install -Dm644 mx.1 {{ prefix }}/share/man/man1/mx.1

demo:
  cargo build --release --bin mx --example demo
  target/release/examples/demo

gif:
  cargo build --release --bin mx --example demo
  nix-shell -p asciinema asciinema-agg tmux --run ./demo.sh

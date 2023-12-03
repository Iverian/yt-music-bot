alias c:= connect

list:
  @just --list

connect:
  nc -U music-server.sock

run *OPTS:
  RUST_BACKTRACE=1 poetry run -- cargo run {{ OPTS }}

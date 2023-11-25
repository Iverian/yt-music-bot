alias c:= connect

list:
  @just --list

connect:
  nc -U music-server.sock

run *OPTS:
  cargo run {{ OPTS }}

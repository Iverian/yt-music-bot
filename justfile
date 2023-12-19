python_version := "3.8"
python_platform := "manylinux_2_5_x86_64"
rust_target := "x86_64-unknown-linux-gnu"
binary := "yt-music-bot"

alias c:= connect

list:
  @just --list

connect:
  nc -U music-server.sock

run *OPTS:
  RUST_BACKTRACE=1 poetry run -- cargo run {{ OPTS }}

python:
  mkdir -p "{{ justfile_directory() }}/dist/python"
  poetry export --without-hashes --format constraints.txt --output "{{ justfile_directory() }}/dist/python/requirements.txt"
  poetry run pip download --isolated --python-version "{{ python_version }}" --platform "{{ python_platform }}" --only-binary=:all: --requirement "{{ justfile_directory() }}/dist/python/requirements.txt" --dest "{{ justfile_directory() }}/dist/python"

build:
  mkdir -p "{{ justfile_directory() }}/dist/bin"
  cross +nightly build --release --target "{{ rust_target }}"
  cp "{{ justfile_directory() }}/target/{{ rust_target }}/release/{{ binary }}" "{{ justfile_directory() }}/dist/bin/{{ binary }}"

package: python build
  mkdir -p "{{ justfile_directory() }}/target/packer"
  tar -c -C "{{ justfile_directory() }}/dist" -f "{{ justfile_directory() }}/package.tar.gz" .

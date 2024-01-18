python_version := "3.11"
python_platform := "manylinux_2_17_x86_64"
rust_target := "x86_64-unknown-linux-gnu"
binary := "yt-music-bot"
build_executable := "cargo"

alias c:= connect

list:
  @just --list

connect:
  nc -U music-server.sock

python-install:
  poetry install --no-root

run *OPTS:
  RUST_BACKTRACE=1 poetry run -- cargo run {{ OPTS }}

clean:
  rm -rf "{{ justfile_directory() }}/dist"

python:
  mkdir -p "{{ justfile_directory() }}/dist/python"
  poetry export --without-hashes --format requirements.txt --output "{{ justfile_directory() }}/dist/python/requirements.txt"
  poetry run pip download --isolated --python-version "{{ python_version }}" --platform "{{ python_platform }}" --only-binary=:all: --requirement "{{ justfile_directory() }}/dist/python/requirements.txt" --dest "{{ justfile_directory() }}/dist/python"

build:
  mkdir -p "{{ justfile_directory() }}/dist/bin"
  {{ build_executable }} +nightly build -Z build-std --release --target "{{ rust_target }}"
  cp "{{ justfile_directory() }}/target/{{ rust_target }}/release/{{ binary }}" "{{ justfile_directory() }}/dist/bin/{{ binary }}"

package: python build
  mkdir -p "{{ justfile_directory() }}/target/packer"
  tar -c -C "{{ justfile_directory() }}/dist" -f "{{ justfile_directory() }}/package.tar.gz" .

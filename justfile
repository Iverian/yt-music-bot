python_version := "3.8"
python_platform := "manylinux_2_5_x86_64"

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
  cargo install --root "{{ justfile_directory() }}/dist" --path "{{ justfile_directory() }}"
  # cross +nightly install --root "{{ justfile_directory() }}/dist" --path "{{ justfile_directory() }}"

package: python build
  mkdir -p "{{ justfile_directory() }}/target/packer"
  tar -c -C "{{ justfile_directory() }}/dist" -f "{{ justfile_directory() }}/package.tar.gz" .

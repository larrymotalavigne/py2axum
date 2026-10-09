#!/bin/sh
# docker run ghcr.io/larrymotalavigne/py2axum build [PACKAGE] [options]   translate and compile (py2axum-build)
# docker run ghcr.io/larrymotalavigne/py2axum check PACKAGE [options]     py2axum check
# docker run ghcr.io/larrymotalavigne/py2axum py2axum ... | bash | ...    any other command
set -e
case "${1:-}" in
  build) shift; exec py2axum-build "$@" ;;
  check) exec py2axum "$@" ;;
  -*) exec py2axum "$@" ;;
  "") exec py2axum-build --help ;;
  *) exec "$@" ;;
esac

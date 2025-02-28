#!/bin/bash

export DOCKER_BUILDKIT=1

root_dir="$(dirname $0)"

docker build \
  --ssh default \
  --progress=plain \
  --platform linux/x86_64 \
  -f Dockerfile-build-wheel.x86_64 \
  -t docker-cja-arrow-dev.dr-uw2.adobeitc.com/datafusion-python:44.0.0-amd64 \
  "$root_dir"

id=$(docker create --platform linux/amd64 docker-cja-arrow-dev.dr-uw2.adobeitc.com/datafusion-python:44.0.0-amd64)
docker cp $id:/datafusion-44.0.0-cp38-abi3-manylinux_2_28_x86_64.whl .
docker rm -v $id

.PHONY: build build-release up down restart logs smoke test test-submission docker-push build-images

DOCKER_COMPOSE = docker-compose
K6_IMAGE = grafana/k6
PWD = $(shell pwd)
API_IMAGE = jaoppb/rinha-2026-rust:latest
DATA_LOADER_IMAGE = jaoppb/rinha-2026-data-loader:latest

# Core build target
build-images:
	docker build -t $(API_IMAGE) --build-arg INPUT_FILE=$(INPUT_FILE) --build-arg CARGO_FEATURES=$(CARGO_FEATURES) .
	docker build -t $(DATA_LOADER_IMAGE) --build-arg INPUT_FILE=$(INPUT_FILE) -f data/Dockerfile .

# Default dev build: verbose profile + example data
build:
	make build-images INPUT_FILE=resources/example-references.json CARGO_FEATURES=verbose

# Release build: no verbose + full data
build-release:
	make build-images INPUT_FILE=resources/references.json.gz CARGO_FEATURES=""

up:
	$(DOCKER_COMPOSE) up -d

down:
	$(DOCKER_COMPOSE) down

restart:
	make down && make build && make up

logs:
	$(DOCKER_COMPOSE) logs -f

smoke:
	docker run --rm --network host -i $(K6_IMAGE) run - <test/smoke.js

test:
	docker run --rm --network host -u root -w /api -v "$(PWD):/api" -i $(K6_IMAGE) run test/test.js

test-submission:
	make down
	make build-release
	make up
	sleep 10
	make test

docker-push: build-release
	docker push $(API_IMAGE)
	docker push $(DATA_LOADER_IMAGE)

run-all: restart
	sleep 5
	make smoke

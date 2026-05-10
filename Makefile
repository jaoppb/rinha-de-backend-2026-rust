.PHONY: build build-release up down restart logs smoke test docker-push

DOCKER_COMPOSE = docker-compose
K6_IMAGE = grafana/k6
PWD = $(shell pwd)
API_IMAGE = jaoppb/rinha-2026-rust:latest
DATA_LOADER_IMAGE = jaoppb/rinha-2026-data-loader:latest

# Default dev build: debug profile + example data
build:
	$(DOCKER_COMPOSE) build --build-arg INPUT_FILE=resources/example-references.json

# Release build: release profile + full data
build-release:
	docker build -t rinha-de-backend-2026-api1 .
	$(DOCKER_COMPOSE) build --build-arg INPUT_FILE=resources/references.json.gz data-loader

up:
	$(DOCKER_COMPOSE) up -d

down:
	$(DOCKER_COMPOSE) down

restart:
	$(DOCKER_COMPOSE) down && make build && $(DOCKER_COMPOSE) up -d

logs:
	$(DOCKER_COMPOSE) logs -f

smoke:
	docker run --rm --network host -i $(K6_IMAGE) run - <test/smoke.js

test:
	docker run --rm --network host -v "$(PWD)/test:/test" -i $(K6_IMAGE) run /test/test.js

docker-push:
	docker build -t $(API_IMAGE) --build-arg INPUT_FILE=resources/references.json.gz .
	docker build -t $(DATA_LOADER_IMAGE) --build-arg INPUT_FILE=resources/references.json.gz -f data/Dockerfile .
	docker push $(API_IMAGE)
	docker push $(DATA_LOADER_IMAGE)

run-all: restart
	sleep 5
	make smoke

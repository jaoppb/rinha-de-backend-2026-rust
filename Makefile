.PHONY: build build-release up down restart logs smoke test test-submission docker-push build-images preview monitor

DOCKER_COMPOSE = docker-compose
K6_IMAGE = grafana/k6
PWD = $(shell pwd)
API_IMAGE = jaoppb/rinha-2026-rust:latest
LB_IMAGE = jaoppb/rinha-2026-lb:latest

# Core build target
build-images:
	docker build -t $(API_IMAGE) --build-arg INPUT_FILE=$(INPUT_FILE) .
	docker build -t $(LB_IMAGE) lb/

# Default dev build: example data
build:
	make build-images INPUT_FILE=resources/example-references.json

# Release build: full data
build-release:
	make build-images INPUT_FILE=resources/references.json.gz

# Build images with verbose-logging feature enabled (tags with :verbose)
build-verbose:
	docker build -t $(API_IMAGE) --build-arg INPUT_FILE=resources/example-references.json --build-arg FEATURES="--features verbose-logging" .
	docker build -t $(LB_IMAGE) --build-arg FEATURES="--features verbose-logging" lb/

build-release-verbose:
	LOG_TRANSPORT=json docker build -t $(API_IMAGE) --build-arg INPUT_FILE=resources/references.json.gz --build-arg FEATURES="--features verbose-logging" .
	LOG_TRANSPORT=json docker build -t $(LB_IMAGE) --build-arg FEATURES="--features verbose-logging" lb/

up:
	LOG_TRANSPORT=json $(DOCKER_COMPOSE) up -d

down:
	$(DOCKER_COMPOSE) down

restart: down build up

logs:
	$(DOCKER_COMPOSE) logs -f

smoke:
	docker run --rm --network host -i $(K6_IMAGE) run - <test/smoke.js

test:
	docker run --rm --network host -u root -w /api -v "$(PWD):/api" -i $(K6_IMAGE) run test/test.js

test-submission: down build-release up test

docker-push: build-release
	docker push $(API_IMAGE)
	docker push $(LB_IMAGE)

run-all: restart smoke

preview:
	gh issue create --repo zanfranceschi/rinha-de-backend-2026 --title "preview" --body "rinha/test jaoppb-rust"

monitor:
	LOG_TRANSPORT=json ./scripts/monitor.sh

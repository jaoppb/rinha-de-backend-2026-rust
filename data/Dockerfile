# Stage 1: Build the binary dataset
FROM python:3.12-slim AS builder

WORKDIR /app
# Accept the input file as a build argument
ARG INPUT_FILE=resources/references.json.gz

# Copy the script
COPY data/convert.py .
# Copy common resources and the specific input file
COPY resources/mcc_risk.json resources/normalization.json ./resources/
COPY $INPUT_FILE ./resources/

# Run the conversion script
# We pass the filename part to the script since it's now in ./resources/
RUN python3 convert.py ./resources/$(basename $INPUT_FILE)

# Stage 2: Final image to serve the data
FROM alpine:latest

# Copy the binaries from the builder stage
COPY --from=builder /app/*.bin /

# Entrypoint: copy binaries to shared memory (/dev/shm) and keep container alive
ENTRYPOINT ["sh", "-c", "cp /*.bin /dev/shm/ && echo 'Datasets loaded to /dev/shm/' && ls -lh /dev/shm/ && tail -f /dev/null"]

# Cross-compile clawx-service.exe for Windows (x86_64-pc-windows-gnu)
# using mingw-w64 on a Linux build host. No Windows machine needed.
#
# Usage (from repo root):
#   docker buildx build -f docker/windows-cross.Dockerfile \
#     --output type=local,dest=./dist .
#
# Result: ./dist/clawx-service.exe

FROM rust:alpine AS builder

RUN apk add --no-cache mingw-w64-gcc
RUN rustup target add x86_64-pc-windows-gnu

WORKDIR /work
COPY . .
RUN cargo build --release --target x86_64-pc-windows-gnu

# Export stage: keep the output image tiny, contains only the binary.
FROM scratch AS export
COPY --from=builder /work/target/x86_64-pc-windows-gnu/release/clawx-service.exe /clawx-service.exe

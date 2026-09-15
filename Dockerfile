# Le démon mailcore, pour la machine dédiée.
#
# Ce conteneur sert le protocole MCP en HTTP. Il ne fait **pas** l'import : le profil
# Thunderbird vit sur le poste de travail, pas ici. Le store lui est monté en volume, déjà
# rempli par `mail import` exécuté là où le profil se trouve.
#
# Construire :  docker build -t mailcore .
# Lancer     :  docker compose up -d      (voir compose.yaml)

# ---------------------------------------------------------------- compilation

FROM rust:1.96-slim-bookworm AS build

# `libsqlite3-sys` est compilé depuis les sources (feature `bundled`), donc il faut un
# compilateur C. `pkg-config` sert à zstd. Rien d'autre : pas d'OpenSSL, `rustls` s'en passe.
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# Les manifestes d'abord, sans les sources : la couche des dépendances ne se reconstruit
# alors que si un Cargo.toml change. Sur un workspace qui tire tantivy, ça fait la différence
# entre une reconstruction de dix secondes et de dix minutes.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/mailcore/Cargo.toml     crates/mailcore/
COPY crates/mailhtml/Cargo.toml     crates/mailhtml/
COPY crates/mailimport/Cargo.toml   crates/mailimport/
COPY crates/mailmcp/Cargo.toml      crates/mailmcp/
COPY crates/maild/Cargo.toml        crates/maild/
COPY crates/mail-cli/Cargo.toml     crates/mail-cli/
COPY crates/mail-ui/Cargo.toml      crates/mail-ui/
COPY xtask/Cargo.toml               xtask/

# Des sources factices, juste de quoi que cargo résolve et compile l'arbre de dépendances.
RUN for crate in mailcore mailhtml mailimport mailmcp; do \
        mkdir -p "crates/$crate/src" && echo '' > "crates/$crate/src/lib.rs"; \
    done \
    && for bin in maild mail-cli mail-ui; do \
        mkdir -p "crates/$bin/src" && echo 'fn main() {}' > "crates/$bin/src/main.rs"; \
    done \
    && mkdir -p xtask/src && echo 'fn main() {}' > xtask/src/main.rs \
    && cargo build --release --package maild \
    && rm -rf crates/*/src xtask/src

COPY crates crates
COPY xtask xtask

# Toucher les sources : sans ça, cargo garde les artefacts factices, dont les horodatages
# sont plus récents que ceux des fichiers qu'on vient de copier.
RUN find crates xtask -name '*.rs' -exec touch {} + \
    && cargo build --release --package maild

# ---------------------------------------------------------------- exécution

FROM debian:bookworm-slim

# Un utilisateur sans privilèges : le conteneur ne lit que le store, il n'a aucune raison
# d'être root. Le store est monté en lecture-écriture — le démon n'écrit pas en phase 1, mais
# SQLite a besoin d'écrire son WAL même pour lire.
RUN groupadd --system --gid 10001 mailcore \
    && useradd --system --uid 10001 --gid mailcore --no-create-home mailcore \
    && mkdir -p /store \
    && chown mailcore:mailcore /store

COPY --from=build /src/target/release/maild /usr/local/bin/maild

USER mailcore
WORKDIR /store
VOLUME ["/store"]

ENV MAILCORE_STORE=/store
EXPOSE 7847

# `0.0.0.0` dans le conteneur, mais **publié sur `127.0.0.1` de l'hôte** par compose : le
# démon ne doit pas se retrouver sur le réseau sans TLS. Le jeton reste obligatoire — le
# démon refuse de démarrer sans (critère 10 de docs/PHASE-1.md), et c'est voulu.
ENTRYPOINT ["maild"]
CMD ["http", "--listen", "0.0.0.0:7847"]

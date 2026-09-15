//! Les choix de transport, et ce qu'ils impliquent.
//!
//! ## Ce qui est servi
//!
//! **stdio**, pour un client qui tourne sur cette machine. Un seul protocole par session —
//! MCP ou l'API — parce qu'il n'y a qu'un flux d'entrée et que rien dans un message ne dirait
//! auquel il appartient. C'est le transport sur lequel on isole un bug du cœur d'un bug du
//! réseau, raison pour laquelle il doit rester fonctionnel en permanence, même quand le
//! déploiement visé est distant.
//!
//! **TCP**, pour un client ailleurs sur le réseau. Il porte les deux protocoles en même
//! temps, sur `/mcp` et `/api` : un chemin d'URL suffit à les distinguer, et un seul port
//! ouvert est un seul port à protéger. Jamais sans jeton, jamais en clair hors bouclage sans
//! que l'opérateur l'ait affirmé — voir [`super::auth`].
//!
//! ## La socket locale nommée, et pourquoi elle n'est pas là
//!
//! `docs/ARCHITECTURE.md` décrivait le transport local comme un *named pipe* sur Windows et
//! une socket Unix ailleurs. En pratique, stdio couvre le même besoin sans code
//! spécifique à la plateforme : le client démarre le démon en processus fils et parle sur les
//! flux qu'il vient d'ouvrir, ce qui lui donne les mêmes garanties — un interlocuteur que
//! seules les permissions du système de fichiers autorisent à exister — pour deux appels
//! système au lieu d'une implémentation par système d'exploitation.
//!
//! Ce qu'une socket nommée apporterait de plus est un démon **déjà lancé** auquel plusieurs
//! clients se rattachent sans le redémarrer. C'est un vrai besoin, mais un besoin de service
//! installé, pas de la phase 1 : d'ici là, le déploiement de référence est un démon distant,
//! et pour lui c'est TCP.
//!
//! ## `rustls` plutôt qu'OpenSSL
//!
//! Pas de dépendance système à installer sur trois plateformes, et le démon n'a pas à
//! interopérer avec un parc existant. `axum-server` termine le TLS lui-même — d'où le retrait
//! de `tokio-rustls`, retenu au départ puis inutilisé.
//!
//! ## Le tunnel reste le déploiement recommandé
//!
//! Pour l'accès distant, un tunnel déjà chiffré et authentifié monté en dehors de mailcore
//! (WireGuard, tunnel SSH) ramène le cas distant au cas local et réduit à zéro la surface
//! d'attaque que nous écrivons nous-mêmes. Le TLS du transport TCP est la ceinture pour qui
//! ne veut pas de tunnel, pas une invitation à exposer le démon sur un réseau hostile.

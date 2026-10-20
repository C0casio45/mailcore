//! La borne de mémoire que le système pose à un processus enfant — `docs/PHASE-4.md`, entrée du
//! 2026-10-20.
//!
//! ## Pourquoi une borne posée par le système
//!
//! L'enfant extracteur lit des PDF écrits par des inconnus. La bibliothèque ne borne la
//! décompression qu'au chargement du fichier — flux d'objets et de références croisées : une bombe
//! placée dans une police ou dans sa table ToUnicode passe outre (relu dans pdf-inspector 1.25.2 et
//! lopdf 0.45). Un relevé périodique de la mémoire de l'enfant arrive trop tard : à quelques Gio/s
//! de décompression, l'enfant engage un gigaoctet entre deux relevés, sur la machine de quelqu'un
//! qui s'en sert. La seule borne qui tienne est celle que le système impose : une allocation
//! au-delà **échoue**, l'enfant avorte — comme pour toute panne qui ne se rattrape pas —, et le
//! parent le lit sur sa sortie d'erreur (« memory allocation of … failed »).
//!
//! ## Pourquoi ce crate porte le seul `unsafe` de mailcore
//!
//! Sous Linux, `prlimit(RLIMIT_DATA)` sur le processus de l'enfant, par rustix : une API sûre.
//! Sous Windows, une limite de mémoire par processus n'existe qu'au travers d'un **objet de
//! travail**, et l'API Win32 n'existe qu'en `unsafe`. Aucun crate ne l'enveloppe pour la mémoire
//! *engagée*. Ces quatre appels sont donc ici, isolés dans un module, chacun avec son contrat, et
//! le reste du projet garde `#![forbid(unsafe_code)]`.
//!
//! Un effet de plus, voulu : l'objet de travail porte `KILL_ON_JOB_CLOSE`. Si le parent meurt, le
//! système ferme sa poignée et tue l'enfant — un extracteur orphelin ne survit pas à la coquille.
//!
//! ## Quand la poser
//!
//! **Entre le lancement et le travail** : l'enfant attend sa requête sur son entrée standard, et le
//! parent pose la borne avant de l'écrire. Ce qui précède — le démarrage de l'exécutable — est sans
//! entrée hostile.

#![deny(unsafe_code)]

/// La borne posée sur un enfant. **À garder en vie** tant que l'enfant travaille : sous Windows,
/// la lâcher ferme l'objet de travail, ce qui tue l'enfant.
#[derive(Debug)]
pub struct Bound {
    #[cfg(windows)]
    _job: windows::Job,
}

/// Pose sur `child` une borne de `bytes` octets de mémoire.
///
/// # Errors
///
/// Si le système refuse — un objet de travail qui ne s'imbrique pas, un `prlimit` refusé —, ou
/// sur un système où la borne n'existe pas : **l'appelant ne doit pas lancer le travail sans elle**.
pub fn bound(child: &std::process::Child, bytes: usize) -> std::io::Result<Bound> {
    #[cfg(windows)]
    {
        Ok(Bound {
            _job: windows::bound(child, bytes)?,
        })
    }
    #[cfg(target_os = "linux")]
    {
        linux::bound(child, bytes)?;
        Ok(Bound {})
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = (child, bytes);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "aucune borne de mémoire par processus sur ce système",
        ))
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use rustix::process::{Pid, Resource, Rlimit, prlimit};

    /// `RLIMIT_DATA` et non `RLIMIT_AS` : depuis Linux 4.7, il compte le tas et les projections
    /// privées en écriture — ce qu'alloue un décompresseur —, sans compter les piles et les arènes
    /// réservées jamais touchées, qui feraient échouer un enfant sain sous `RLIMIT_AS`.
    pub(super) fn bound(child: &std::process::Child, bytes: usize) -> std::io::Result<()> {
        let pid = i32::try_from(child.id())
            .ok()
            .and_then(Pid::from_raw)
            .ok_or_else(|| std::io::Error::other("identifiant de processus hors bornes"))?;
        let limit = u64::try_from(bytes).unwrap_or(u64::MAX);
        prlimit(
            Some(pid),
            Resource::Data,
            Rlimit {
                current: Some(limit),
                maximum: Some(limit),
            },
        )?;
        Ok(())
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };

    /// Un objet de travail, dont la poignée se ferme une fois.
    #[derive(Debug)]
    pub(super) struct Job(HANDLE);

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: `self.0` est la poignée rendue par `CreateJobObjectW`, possédée par ce seul
            // `Job`, jamais copiée, et fermée ici une seule fois.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    pub(super) fn bound(child: &std::process::Child, bytes: usize) -> std::io::Result<Job> {
        // SAFETY: des attributs de sécurité nuls et un nom nul sont documentés comme valides —
        // un objet anonyme, à la sécurité par défaut, que personne d'autre ne peut ouvrir.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let job = Job(handle);

        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_PROCESS_MEMORY | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        info.ProcessMemoryLimit = bytes;
        let size = u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
            .map_err(|_| std::io::Error::other("taille de structure hors bornes"))?;
        // SAFETY: la poignée est vivante ; le pointeur désigne une structure vivante, de la taille
        // annoncée, du type que cette classe d'information attend ; Windows la lit et ne la garde
        // pas au-delà de l'appel.
        let set = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                size,
            )
        };
        if set == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: la poignée de l'enfant est celle que `std::process::Child` possède, valide tant
        // que `child` vit — il est emprunté pendant l'appel ; Windows n'en prend pas la propriété.
        let assigned = unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle()) };
        if assigned == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(job)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::io::{BufRead, Write};
    use std::process::{Command, Stdio};

    /// Le rôle d'enfant de ce binaire de test : attendre la consigne, puis allouer et toucher
    /// `MAILBOUND_ALLOCATE` octets. Sans la variable, ce test ne fait rien.
    #[test]
    fn allocating_child() {
        let Some(bytes) = std::env::var("MAILBOUND_ALLOCATE")
            .ok()
            .and_then(|it| it.parse::<usize>().ok())
        else {
            return;
        };
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line).unwrap();
        let mut block = vec![0u8; bytes];
        for page in block.chunks_mut(4096) {
            page[0] = 1;
        }
        assert!(block.iter().step_by(4096).all(|&b| b == 1));
    }

    /// Lance ce même binaire en enfant qui alloue `bytes`, borné à `limit` s'il en a une.
    fn child_allocating(bytes: usize, limit: Option<usize>) -> bool {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "tests::allocating_child",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("MAILBOUND_ALLOCATE", bytes.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let bound = limit.map(|limit| super::bound(&child, limit).unwrap());
        child.stdin.take().unwrap().write_all(b"go\n").unwrap();
        let status = child.wait().unwrap();
        drop(bound);
        status.success()
    }

    /// Au-delà de la borne, l'allocation échoue et l'enfant meurt.
    #[test]
    fn a_child_past_its_bound_dies() {
        assert!(
            !child_allocating(512 * 1024 * 1024, Some(128 * 1024 * 1024)),
            "l'enfant a alloué 512 Mio sous une borne de 128 Mio"
        );
    }

    /// Le contrôle inverse : la même allocation, sans borne ou sous une borne assez haute, passe —
    /// sans lui, « l'enfant meurt » pourrait vouloir dire « l'enfant ne marche pas ».
    #[test]
    fn the_same_child_lives_without_a_bound_or_under_a_high_one() {
        assert!(child_allocating(512 * 1024 * 1024, None));
        assert!(child_allocating(64 * 1024 * 1024, Some(512 * 1024 * 1024)));
    }
}

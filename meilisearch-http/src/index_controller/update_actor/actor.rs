use std::collections::HashSet;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use log::info;
use oxidized_json_checker::JsonChecker;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use uuid::Uuid;

use super::{PayloadData, Result, UpdateError, UpdateMsg, UpdateStore, UpdateStoreInfo};
use crate::index_controller::index_actor::IndexActorHandle;
use crate::index_controller::{UpdateMeta, UpdateStatus};

pub struct UpdateActor<D, I> {
    path: PathBuf,
    store: Arc<UpdateStore>,
    inbox: mpsc::Receiver<UpdateMsg<D>>,
    index_handle: I,
    must_exit: Arc<AtomicBool>,
}

impl<D, I> UpdateActor<D, I>
where
    D: AsRef<[u8]> + Sized + 'static,
    I: IndexActorHandle + Clone + Send + Sync + 'static,
{
    pub fn new(
        update_db_size: usize,
        inbox: mpsc::Receiver<UpdateMsg<D>>,
        path: impl AsRef<Path>,
        index_handle: I,
    ) -> anyhow::Result<Self> {
        let path = path.as_ref().join("updates");

        std::fs::create_dir_all(&path)?;

        let mut options = heed::EnvOpenOptions::new();
        options.map_size(update_db_size);

        let must_exit = Arc::new(AtomicBool::new(false));

        let store = UpdateStore::open(options, &path, index_handle.clone(), must_exit.clone())?;
        std::fs::create_dir_all(path.join("update_files"))?;

        Ok(Self {
            path,
            store,
            inbox,
            index_handle,
            must_exit,
        })
    }

    pub async fn run(mut self) {
        use UpdateMsg::*;

        info!("Started update actor.");

        loop {
            let msg = self.inbox.recv().await;

            if self.must_exit.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }

            match msg {
                Some(Update {
                    uuid,
                    meta,
                    data,
                    ret,
                }) => {
                    let _ = ret.send(self.handle_update(uuid, meta, data).await);
                }
                Some(ListUpdates { uuid, ret }) => {
                    let _ = ret.send(self.handle_list_updates(uuid).await);
                }
                Some(GetUpdate { uuid, ret, id }) => {
                    let _ = ret.send(self.handle_get_update(uuid, id).await);
                }
                Some(Delete { uuid, ret }) => {
                    let _ = ret.send(self.handle_delete(uuid).await);
                }
                Some(Snapshot { uuids, path, ret }) => {
                    let _ = ret.send(self.handle_snapshot(uuids, path).await);
                }
                Some(Dump { uuids, path, ret }) => {
                    let _ = ret.send(self.handle_dump(uuids, path).await);
                }
                Some(GetInfo { ret }) => {
                    let _ = ret.send(self.handle_get_info().await);
                }
                None => break,
            }
        }
    }

    async fn handle_update(
        &self,
        uuid: Uuid,
        meta: UpdateMeta,
        mut payload: mpsc::Receiver<PayloadData<D>>,
    ) -> Result<UpdateStatus> {
        let file_path = match meta {
            UpdateMeta::DocumentsAddition { .. } | UpdateMeta::DeleteDocuments => {
                let update_file_id = uuid::Uuid::new_v4();
                let path = self
                    .path
                    .join(format!("update_files/update_{}", update_file_id));
                let mut file = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .open(&path)
                    .await?;

                let mut file_len = 0;
                while let Some(bytes) = payload.recv().await {
                    let bytes = bytes?;
                    file_len += bytes.as_ref().len();
                    file.write_all(bytes.as_ref()).await?;
                }

                if file_len != 0 {
                    file.flush().await?;
                    let file = file.into_std().await;
                    Some((file, update_file_id))
                } else {
                    // empty update, delete the empty file.
                    fs::remove_file(&path).await?;
                    None
                }
            }
            _ => None,
        };

        let update_store = self.store.clone();

        tokio::task::spawn_blocking(move || {
            use std::io::{copy, sink, BufReader, Seek};

            // If the payload is empty, ignore the check.
            let update_uuid = if let Some((mut file, uuid)) = file_path {
                // set the file back to the beginning
                file.seek(SeekFrom::Start(0))?;
                // Check that the json payload is valid:
                let reader = BufReader::new(&mut file);
                let mut checker = JsonChecker::new(reader);

                if copy(&mut checker, &mut sink()).is_err() || checker.finish().is_err() {
                    // The json file is invalid, we use Serde to get a nice error message:
                    file.seek(SeekFrom::Start(0))?;
                    let _: serde_json::Value = serde_json::from_reader(file)?;
                }
                Some(uuid)
            } else {
                None
            };

            // The payload is valid, we can register it to the update store.
            let status = update_store
                .register_update(meta, update_uuid, uuid)
                .map(UpdateStatus::Enqueued)?;
            Ok(status)
        })
        .await?
    }

    async fn handle_list_updates(&self, uuid: Uuid) -> Result<Vec<UpdateStatus>> {
        let update_store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let result = update_store.list(uuid)?;
            Ok(result)
        })
        .await?
    }

    async fn handle_get_update(&self, uuid: Uuid, id: u64) -> Result<UpdateStatus> {
        let store = self.store.clone();
        let result = store
            .meta(uuid, id)?
            .ok_or(UpdateError::UnexistingUpdate(id))?;
        Ok(result)
    }

    async fn handle_delete(&self, uuid: Uuid) -> Result<()> {
        let store = self.store.clone();

        tokio::task::spawn_blocking(move || store.delete_all(uuid)).await??;

        Ok(())
    }

    async fn handle_snapshot(&self, uuids: HashSet<Uuid>, path: PathBuf) -> Result<()> {
        let index_handle = self.index_handle.clone();
        let update_store = self.store.clone();

        tokio::task::spawn_blocking(move || update_store.snapshot(&uuids, &path, index_handle))
            .await??;

        Ok(())
    }

    async fn handle_dump(&self, uuids: HashSet<Uuid>, path: PathBuf) -> Result<()> {
        let index_handle = self.index_handle.clone();
        let update_store = self.store.clone();

        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            update_store.dump(&uuids, path.to_path_buf(), index_handle)?;
            Ok(())
        })
        .await??;

        Ok(())
    }

    async fn handle_get_info(&self) -> Result<UpdateStoreInfo> {
        let update_store = self.store.clone();
        let info = tokio::task::spawn_blocking(move || -> anyhow::Result<UpdateStoreInfo> {
            let info = update_store.get_info()?;
            Ok(info)
        })
        .await??;

        Ok(info)
    }
}

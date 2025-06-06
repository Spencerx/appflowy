use crate::entities::icon::UpdateViewIconParams;
use crate::entities::{
  AFAccessLevelPB, CreateViewParams, DeletedViewPB, DuplicateViewParams, FolderSnapshotPB,
  MoveNestedViewParams, RepeatedSharedViewResponsePB, RepeatedTrashPB, RepeatedViewIdPB,
  RepeatedViewPB, SharedViewPB, SharedViewSectionPB, UpdateViewParams, ViewLayoutPB, ViewPB,
  ViewSectionPB, WorkspaceLatestPB, WorkspacePB, view_pb_with_all_child_views,
  view_pb_with_child_views, view_pb_without_child_views, view_pb_without_child_views_from_arc,
};
use crate::manager_observer::{
  ChildViewChangeReason, notify_child_views_changed, notify_did_update_workspace,
  notify_parent_view_did_change,
};
use crate::notification::{FolderNotification, folder_notification_builder};
use crate::publish_util::{generate_publish_name, view_pb_to_publish_view};
use crate::share::{ImportData, ImportItem, ImportParams};
use crate::util::{folder_not_init_error, workspace_data_not_sync_error};
use crate::view_operation::{
  FolderOperationHandler, FolderOperationHandlers, GatherEncodedCollab, ViewData, create_view,
};
use arc_swap::ArcSwapOption;
use client_api::entity::PublishInfo;
use client_api::entity::guest_dto::{
  RevokeSharedViewAccessRequest, ShareViewWithGuestRequest, SharedViewDetails,
};
use client_api::entity::workspace_dto::PublishInfoView;
use collab::core::collab::{DataSource, IndexContentReceiver};
use collab::lock::RwLock;
use collab_entity::{CollabType, EncodedCollab};
use collab_folder::folder_diff::FolderViewChange;
use collab_folder::hierarchy_builder::{ParentChildViews, ViewExtraBuilder};
use collab_folder::{
  Folder, FolderData, FolderNotify, Section, SectionItem, SpacePermission, TrashInfo, View,
  ViewLayout, ViewUpdate, Workspace,
};
use collab_integrate::CollabKVDB;
use collab_integrate::collab_builder::{
  AppFlowyCollabBuilder, CollabBuilderConfig, CollabPersistenceImpl,
};
use flowy_error::{ErrorCode, FlowyError, FlowyResult, internal_error};
use flowy_folder_pub::cloud::{FolderCloudService, FolderCollabParams, gen_view_id};
use flowy_folder_pub::entities::{
  PublishDatabaseData, PublishDatabasePayload, PublishDocumentPayload, PublishPayload,
  PublishViewInfo, PublishViewMeta, PublishViewMetaData,
};
use flowy_folder_pub::sql::workspace_shared_view_sql::{
  WorkspaceSharedViewTable, replace_all_workspace_shared_views, select_all_workspace_shared_views,
};
use flowy_sqlite::DBConnection;
use flowy_sqlite::kv::KVStorePreferences;
use flowy_user_pub::entities::{Role, UserWorkspace};
use futures::future;
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::str::FromStr;
use std::sync::{Arc, Weak};
use tokio::sync::RwLockWriteGuard;
use tracing::{error, info, instrument};
use uuid::Uuid;

pub trait FolderUser: Send + Sync {
  fn user_id(&self) -> Result<i64, FlowyError>;
  fn workspace_id(&self) -> Result<Uuid, FlowyError>;
  fn collab_db(&self, uid: i64) -> Result<Weak<CollabKVDB>, FlowyError>;
  fn sqlite_connection(&self, uid: i64) -> Result<DBConnection, FlowyError>;
  fn is_folder_exist_on_disk(&self, uid: i64, workspace_id: &Uuid) -> FlowyResult<bool>;
  fn get_active_user_workspace(&self) -> FlowyResult<UserWorkspace>;
}

pub struct FolderManager {
  pub(crate) mutex_folder: ArcSwapOption<RwLock<Folder>>,
  pub(crate) collab_builder: Arc<AppFlowyCollabBuilder>,
  pub(crate) user: Arc<dyn FolderUser>,
  pub(crate) operation_handlers: FolderOperationHandlers,
  pub cloud_service: Weak<dyn FolderCloudService>,
  pub(crate) store_preferences: Arc<KVStorePreferences>,
  pub(crate) folder_ready_notifier: tokio::sync::watch::Sender<bool>,
}

impl Drop for FolderManager {
  fn drop(&mut self) {
    tracing::trace!("[Drop] drop folder manager");
  }
}

impl FolderManager {
  pub fn new(
    user: Arc<dyn FolderUser>,
    collab_builder: Arc<AppFlowyCollabBuilder>,
    cloud_service: Weak<dyn FolderCloudService>,
    store_preferences: Arc<KVStorePreferences>,
  ) -> FlowyResult<Self> {
    let (folder_ready_notifier, _) = tokio::sync::watch::channel(false);
    let manager = Self {
      user,
      mutex_folder: Default::default(),
      collab_builder,
      operation_handlers: Default::default(),
      cloud_service,
      store_preferences,
      folder_ready_notifier,
    };

    Ok(manager)
  }

  pub fn subscribe_folder_ready_notifier(&self) -> tokio::sync::watch::Receiver<bool> {
    self.folder_ready_notifier.subscribe()
  }

  pub fn cloud_service(&self) -> FlowyResult<Arc<dyn FolderCloudService>> {
    self
      .cloud_service
      .upgrade()
      .ok_or_else(FlowyError::ref_drop)
  }

  pub fn register_operation_handler(
    &self,
    layout: ViewLayout,
    handler: Arc<dyn FolderOperationHandler + Send + Sync>,
  ) {
    self.operation_handlers.insert(layout, handler);
  }

  #[instrument(level = "debug", skip(self), err)]
  pub async fn get_current_workspace(&self) -> FlowyResult<WorkspacePB> {
    let workspace_id = self.user.workspace_id()?;
    match self.mutex_folder.load_full() {
      None => {
        let uid = self.user.user_id()?;
        Err(workspace_data_not_sync_error(uid, &workspace_id))
      },
      Some(lock) => {
        let folder = lock.read().await;
        let workspace_pb_from_workspace = |workspace: Workspace, folder: &Folder| {
          let views = get_workspace_public_view_pbs(&workspace_id, folder);
          let workspace: WorkspacePB = (workspace, views).into();
          Ok::<WorkspacePB, FlowyError>(workspace)
        };

        match folder.get_workspace_info(&workspace_id.to_string()) {
          None => Err(FlowyError::record_not_found().with_context("Can not find the workspace")),
          Some(workspace) => workspace_pb_from_workspace(workspace, &folder),
        }
      },
    }
  }

  pub async fn get_folder_data(&self) -> FlowyResult<FolderData> {
    let workspace_id = self.user.workspace_id()?;
    let data = self
      .mutex_folder
      .load_full()
      .ok_or_else(|| internal_error("The folder is not initialized"))?
      .read()
      .await
      .get_folder_data(&workspace_id.to_string())
      .ok_or_else(|| internal_error("Workspace id not match the id in current folder"))?;
    Ok(data)
  }

  pub async fn gather_publish_encode_collab(
    &self,
    view_id: &Uuid,
    layout: &ViewLayout,
  ) -> FlowyResult<GatherEncodedCollab> {
    let handler = self.get_handler(layout)?;
    let encoded_collab = handler
      .gather_publish_encode_collab(&self.user, view_id)
      .await?;
    Ok(encoded_collab)
  }

  /// Return a list of views of the current workspace.
  /// Only the first level of child views are included.
  pub async fn get_current_workspace_public_views(&self) -> FlowyResult<Vec<ViewPB>> {
    let views = self.get_workspace_public_views().await?;
    Ok(views)
  }

  pub async fn get_workspace_public_views(&self) -> FlowyResult<Vec<ViewPB>> {
    let workspace_id = self.user.workspace_id()?;
    match self.mutex_folder.load_full() {
      None => Ok(Vec::default()),
      Some(lock) => {
        let folder = lock.read().await;
        Ok(get_workspace_public_view_pbs(&workspace_id, &folder))
      },
    }
  }

  pub async fn get_workspace_private_views(&self) -> FlowyResult<Vec<ViewPB>> {
    let workspace_id = self.user.workspace_id()?;
    match self.mutex_folder.load_full() {
      None => Ok(Vec::default()),
      Some(folder) => {
        let folder = folder.read().await;
        Ok(get_workspace_private_view_pbs(&workspace_id, &folder))
      },
    }
  }

  #[instrument(level = "trace", skip_all, err)]
  pub(crate) async fn make_folder<T: Into<Option<FolderNotify>>>(
    &self,
    uid: i64,
    workspace_id: &Uuid,
    collab_db: Weak<CollabKVDB>,
    data_source: Option<DataSource>,
    folder_notifier: T,
  ) -> Result<Arc<RwLock<Folder>>, FlowyError> {
    let folder_notifier = folder_notifier.into();
    // only need the check the workspace id when the doc state is not from the disk.
    let config = CollabBuilderConfig::default().sync_enable(true);

    let data_source = data_source.unwrap_or_else(|| {
      CollabPersistenceImpl::new(collab_db.clone(), uid, *workspace_id).into_data_source()
    });

    let object_id = workspace_id;
    let collab_object =
      self
        .collab_builder
        .collab_object(workspace_id, uid, object_id, CollabType::Folder)?;
    let result = self
      .collab_builder
      .create_folder(
        collab_object,
        data_source,
        collab_db,
        config,
        folder_notifier,
        None,
      )
      .await;

    // If opening the folder fails due to missing required data (indicated by a `FolderError::NoRequiredData`),
    // the function logs an informational message and attempts to clear the folder data by deleting its
    // document from the collaborative database. It then returns the encountered error.
    match result {
      Ok(folder) => Ok(folder),
      Err(err) => {
        info!(
          "Clear the folder data and try to open the folder again due to: {}",
          err
        );

        if let Some(db) = self.user.collab_db(uid).ok().and_then(|a| a.upgrade()) {
          let _ = db
            .delete_doc(uid, &workspace_id.to_string(), &object_id.to_string())
            .await;
        }
        Err(err.into())
      },
    }
  }

  pub(crate) async fn create_folder_with_data(
    &self,
    uid: i64,
    workspace_id: &Uuid,
    collab_db: Weak<CollabKVDB>,
    notifier: Option<FolderNotify>,
    folder_data: Option<FolderData>,
  ) -> Result<Arc<RwLock<Folder>>, FlowyError> {
    let object_id = workspace_id;
    let collab_object =
      self
        .collab_builder
        .collab_object(workspace_id, uid, object_id, CollabType::Folder)?;

    let doc_state =
      CollabPersistenceImpl::new(collab_db.clone(), uid, *workspace_id).into_data_source();
    let folder = self
      .collab_builder
      .create_folder(
        collab_object,
        doc_state,
        collab_db,
        CollabBuilderConfig::default().sync_enable(true),
        notifier,
        folder_data,
      )
      .await?;
    Ok(folder)
  }

  /// Initialize the folder with the given workspace id.
  /// Fetch the folder updates from the cloud service and initialize the folder.
  #[tracing::instrument(skip_all, err)]
  pub async fn initialize_after_sign_in(
    &self,
    user_id: i64,
    data_source: FolderInitDataSource,
  ) -> FlowyResult<()> {
    let workspace_id = self.user.workspace_id()?;
    if let Err(err) = self.initialize(user_id, &workspace_id, data_source).await {
      // If failed to open folder with remote data, open from local disk. After open from the local
      // disk. the data will be synced to the remote server.
      error!(
        "initialize folder for user {} with workspace {} encountered error: {:?}, fallback local",
        user_id, workspace_id, err
      );
      self
        .initialize(
          user_id,
          &workspace_id,
          FolderInitDataSource::LocalDisk {
            create_if_not_exist: false,
          },
        )
        .await?;
    }

    Ok(())
  }

  pub async fn initialize_after_open_workspace(
    &self,
    uid: i64,
    data_source: FolderInitDataSource,
  ) -> FlowyResult<()> {
    self.initialize_after_sign_in(uid, data_source).await
  }

  pub async fn subscribe_folder_change_rx(&self) -> FlowyResult<IndexContentReceiver> {
    let folder = self
      .mutex_folder
      .load_full()
      .ok_or_else(folder_not_init_error)?;
    let read_guard = folder.read().await;
    Ok(read_guard.subscribe_index_content())
  }

  pub async fn consumer_recent_workspace_changes(&self) -> FlowyResult<Vec<FolderViewChange>> {
    let folder = self
      .mutex_folder
      .load_full()
      .ok_or_else(folder_not_init_error)?;
    let workspace_id = self.user.workspace_id()?.to_string();
    let encoded_collab = self
      .store_preferences
      .get_object::<EncodedCollab>(&workspace_id);

    if encoded_collab.is_none() {
      return Ok(vec![]);
    }

    let folder = folder.read().await;
    let changes = folder.calculate_view_changes(encoded_collab.unwrap())?;

    let encoded_collab = folder.encode_collab();
    if let Ok(encoded) = encoded_collab {
      let _ = self.store_preferences.set_object(&workspace_id, &encoded);
    }
    Ok(changes)
  }

  pub async fn on_workspace_deleted(&self, _uid: i64, _workspace_id: &Uuid) -> FlowyResult<()> {
    Ok(())
  }

  /// Initialize the folder for the new user.
  /// Using the [DefaultFolderBuilder] to create the default workspace for the new user.
  #[instrument(level = "info", skip_all, err)]
  pub async fn initialize_after_sign_up(
    &self,
    user_id: i64,
    _token: &str,
    is_new: bool,
    data_source: FolderInitDataSource,
    workspace_id: &Uuid,
  ) -> FlowyResult<()> {
    // Create the default workspace if the user is new
    info!("initialize_when_sign_up: is_new: {}", is_new);
    if is_new {
      self.initialize(user_id, workspace_id, data_source).await?;
    } else {
      // The folder updates should not be empty, as the folder data is stored
      // when the user signs up for the first time.
      let result = self
        .cloud_service()?
        .get_folder_doc_state(workspace_id, user_id, CollabType::Folder, workspace_id)
        .await;

      match result {
        Ok(folder_doc_state) => {
          info!(
            "Get folder updates via {}, doc state len: {}",
            self.cloud_service()?.service_name(),
            folder_doc_state.len()
          );
          self
            .initialize(
              user_id,
              workspace_id,
              FolderInitDataSource::Cloud(folder_doc_state),
            )
            .await?;
        },
        Err(err) => {
          if err.is_record_not_found() {
            self.initialize(user_id, workspace_id, data_source).await?;
          } else {
            return Err(err);
          }
        },
      }
    }
    Ok(())
  }

  /// Called when the current user logout
  ///
  pub async fn clear(&self, _user_id: i64) {}

  pub async fn get_workspace_setting_pb(&self) -> FlowyResult<WorkspaceLatestPB> {
    let workspace_id = self.user.workspace_id()?;
    let latest_view = self.get_current_view().await;
    Ok(WorkspaceLatestPB {
      workspace_id: workspace_id.to_string(),
      latest_view,
    })
  }

  /// All the views will become a space under the workspace.
  pub async fn insert_views_as_spaces(
    &self,
    mut views: Vec<ParentChildViews>,
    orphan_views: Vec<ParentChildViews>,
  ) -> Result<(), FlowyError> {
    let lock = self
      .mutex_folder
      .load_full()
      .ok_or_else(|| FlowyError::internal().with_context("The folder is not initialized"))?;
    let mut folder = lock.write().await;
    let workspace_id = folder
      .get_workspace_id()
      .ok_or_else(|| FlowyError::internal().with_context("Cannot find the workspace ID"))?;

    views.iter_mut().for_each(|view| {
      view.view.parent_view_id.clone_from(&workspace_id);
      view.view.extra =
        Some(serde_json::to_string(&ViewExtraBuilder::new().is_space(true).build()).unwrap());
    });
    let all_views = views.into_iter().chain(orphan_views.into_iter()).collect();
    folder.insert_nested_views(all_views);

    Ok(())
  }

  /// Inserts parent-child views into the folder. If a `parent_view_id` is provided,
  /// it will be used to set the `parent_view_id` for all child views. If not, the latest
  /// view (by `last_edited_time`) from the workspace will be used as the parent view.
  ///
  #[instrument(level = "info", skip_all, err)]
  pub async fn insert_views_with_parent(
    &self,
    mut views: Vec<ParentChildViews>,
    orphan_views: Vec<ParentChildViews>,
    parent_view_id: Option<String>,
  ) -> Result<(), FlowyError> {
    let lock = self
      .mutex_folder
      .load_full()
      .ok_or_else(|| FlowyError::internal().with_context("The folder is not initialized"))?;

    // Obtain a write lock on the folder.
    let mut folder = lock.write().await;
    let parent_view_id = parent_view_id.as_deref().filter(|id| !id.is_empty());
    // Set the parent view ID for the child views.
    if let Some(parent_view_id) = parent_view_id {
      // If a valid parent_view_id is provided, set it for each child view.
      if folder.get_view(parent_view_id).is_some() {
        info!(
          "[AppFlowyData]: Attach parent-child views with the latest view: {:?}",
          parent_view_id
        );
        views.iter_mut().for_each(|child_view| {
          child_view.view.parent_view_id = parent_view_id.to_string();
        });
      } else {
        error!(
          "[AppFlowyData]: The provided parent_view_id: {} is not found in the folder",
          parent_view_id
        );
        Self::insert_into_latest_view(&mut views, &mut folder)?;
      }
    } else {
      // If no parent_view_id is provided, find the latest view in the workspace.
      Self::insert_into_latest_view(&mut views, &mut folder)?;
    }

    // Insert the views into the folder.
    let all_views = views.into_iter().chain(orphan_views.into_iter()).collect();
    folder.insert_nested_views(all_views);
    Ok(())
  }

  #[instrument(level = "info", skip_all, err)]
  fn insert_into_latest_view(
    views: &mut [ParentChildViews],
    folder: &mut RwLockWriteGuard<Folder>,
  ) -> Result<(), FlowyError> {
    let workspace_id = folder
      .get_workspace_id()
      .ok_or_else(|| FlowyError::internal().with_context("Cannot find the workspace ID"))?;

    // Get the latest view based on the last_edited_time in the workspace.
    match folder
      .get_views_belong_to(&workspace_id)
      .iter()
      .max_by_key(|view| view.last_edited_time)
    {
      None => info!("[AppFlowyData]: No views found in the workspace"),
      Some(latest_view) => {
        info!(
          "[AppFlowyData]: Attach parent-child views with the latest view: {}:{}, is_space: {:?}",
          latest_view.id,
          latest_view.name,
          latest_view.space_info(),
        );
        views.iter_mut().for_each(|child_view| {
          child_view.view.parent_view_id.clone_from(&latest_view.id);
        });
      },
    }
    Ok(())
  }

  pub async fn get_workspace_pb(&self) -> FlowyResult<WorkspacePB> {
    let workspace_id = self.user.workspace_id()?;
    let lock = self
      .mutex_folder
      .load_full()
      .ok_or_else(|| FlowyError::internal().with_context("folder is not initialized"))?;
    let folder = lock.read().await;
    let workspace = folder
      .get_workspace_info(&workspace_id.to_string())
      .ok_or_else(|| FlowyError::record_not_found().with_context("Can not find the workspace"))?;

    let views = folder
      .get_views_belong_to(&workspace.id)
      .into_iter()
      .map(|view| view_pb_without_child_views(view.as_ref().clone()))
      .collect::<Vec<ViewPB>>();

    Ok(WorkspacePB {
      id: workspace.id,
      name: workspace.name,
      views,
      create_time: workspace.created_at,
    })
  }

  /// Asynchronously creates a view with provided parameters and notifies the workspace if update is needed.
  ///
  /// Commonly, the notify_workspace_update parameter is set to true when the view is created in the workspace.
  /// If you're handling multiple views in the same hierarchy and want to notify the workspace only after the last view is created,
  ///   you can set notify_workspace_update to false to avoid multiple notifications.
  pub async fn create_view_with_params(
    &self,
    params: CreateViewParams,
    notify_workspace_update: bool,
  ) -> FlowyResult<(View, Option<EncodedCollab>)> {
    let workspace_id = self.user.workspace_id()?;
    let view_layout: ViewLayout = params.layout.clone().into();
    let handler = self.get_handler(&view_layout)?;
    let user_id = self.user.user_id()?;
    let mut encoded_collab: Option<EncodedCollab> = None;

    info!(
      "{} create view {}, name:{}, layout:{:?}",
      handler.name(),
      params.view_id,
      params.name,
      params.layout
    );
    if params.meta.is_empty() && params.initial_data.is_empty() {
      handler
        .create_default_view(
          user_id,
          &params.parent_view_id,
          &params.view_id,
          &params.name,
          view_layout.clone(),
        )
        .await?;
    } else {
      encoded_collab = handler
        .create_view_with_view_data(user_id, params.clone())
        .await?;
    }

    let index = params.index;
    let section = params.section.clone().unwrap_or(ViewSectionPB::Public);
    let is_private = section == ViewSectionPB::Private;
    let view = create_view(self.user.user_id()?, params, view_layout);
    if let Some(lock) = self.mutex_folder.load_full() {
      let mut folder = lock.write().await;
      folder.insert_view(view.clone(), index);
      if is_private {
        folder.add_private_view_ids(vec![view.id.clone()]);
      }
      if notify_workspace_update {
        notify_did_update_workspace(&workspace_id, &folder);
      }
    }

    Ok((view, encoded_collab))
  }

  /// The orphan view is meant to be a view that is not attached to any parent view. By default, this
  /// view will not be shown in the view list unless it is attached to a parent view that is shown in
  /// the view list.
  pub async fn create_orphan_view_with_params(
    &self,
    params: CreateViewParams,
  ) -> FlowyResult<View> {
    let view_layout: ViewLayout = params.layout.clone().into();
    // TODO(nathan): remove orphan view. Just use for create document in row
    let handler = self.get_handler(&view_layout)?;
    let user_id = self.user.user_id()?;
    handler
      .create_default_view(
        user_id,
        &params.parent_view_id,
        &params.view_id,
        &params.name,
        view_layout.clone(),
      )
      .await?;

    let view = create_view(self.user.user_id()?, params, view_layout);
    if let Some(lock) = self.mutex_folder.load_full() {
      let mut folder = lock.write().await;
      folder.insert_view(view.clone(), None);
    }
    Ok(view)
  }

  #[tracing::instrument(level = "debug", skip(self), err)]
  pub(crate) async fn close_view(&self, view_id: &str) -> Result<(), FlowyError> {
    if let Some(lock) = self.mutex_folder.load_full() {
      let folder = lock.read().await;
      if let Some(view) = folder.get_view(view_id) {
        // Drop the folder lock explicitly to avoid deadlock when following calls contains 'self'
        drop(folder);

        let view_id = Uuid::from_str(view_id)?;
        let handler = self.get_handler(&view.layout)?;
        handler.close_view(&view_id).await?;
      }
    }
    Ok(())
  }

  /// Retrieves the view corresponding to the specified view ID.
  ///
  /// It is important to note that if the target view contains child views,
  /// this method only provides access to the first level of child views.
  ///
  /// Therefore, to access a nested child view within one of the initial child views, you must invoke this method
  /// again using the ID of the child view you wish to access.
  #[tracing::instrument(level = "debug", skip(self))]
  pub async fn get_view_pb(&self, view_id: &str) -> FlowyResult<ViewPB> {
    let workspace = self.user.get_active_user_workspace()?;
    let role = workspace.role;

    // If the user is a Guest, check if they have access to this view through shared views
    if let Some(Role::Guest) = role {
      let flatten_shared_views = self.get_flatten_shared_pages().await?;
      let has_access = flatten_shared_views
        .iter()
        .any(|shared_view| shared_view.id == view_id);

      if !has_access {
        return Err(FlowyError::new(
          ErrorCode::RecordNotFound,
          format!("Guest user does not have access to view: {}", view_id),
        ));
      }
    }

    let view_id = view_id.to_string();

    let lock = self
      .mutex_folder
      .load_full()
      .ok_or_else(folder_not_init_error)?;
    let folder = lock.read().await;

    // trash views and other private views should not be accessed
    let view_ids_should_be_filtered = Self::get_view_ids_should_be_filtered(&folder);

    if view_ids_should_be_filtered.contains(&view_id) {
      return Err(FlowyError::new(
        ErrorCode::RecordNotFound,
        format!("View: {} is in trash or other private sections", view_id),
      ));
    }

    match folder.get_view(&view_id) {
      None => {
        error!("Can't find the view with id: {}", view_id);
        Err(FlowyError::record_not_found())
      },
      Some(view) => {
        let child_views = folder
          .get_views_belong_to(&view.id)
          .into_iter()
          .filter(|view| !view_ids_should_be_filtered.contains(&view.id))
          .collect::<Vec<_>>();
        let view_pb = view_pb_with_child_views(view, child_views);
        Ok(view_pb)
      },
    }
  }

  /// Retrieves the views corresponding to the specified view IDs.
  ///
  /// It is important to note that if the target view contains child views,
  /// this method only provides access to the first level of child views.
  ///
  /// Therefore, to access a nested child view within one of the initial child views, you must invoke this method
  /// again using the ID of the child view you wish to access.
  #[tracing::instrument(level = "debug", skip(self))]
  pub async fn get_view_pbs_without_children(
    &self,
    view_ids: Vec<String>,
  ) -> FlowyResult<Vec<ViewPB>> {
    let lock = self
      .mutex_folder
      .load_full()
      .ok_or_else(folder_not_init_error)?;

    // trash views and other private views should not be accessed
    let folder = lock.read().await;
    let view_ids_should_be_filtered = Self::get_view_ids_should_be_filtered(&folder);

    let views = view_ids
      .into_iter()
      .filter_map(|view_id| {
        if view_ids_should_be_filtered.contains(&view_id) {
          return None;
        }
        folder.get_view(&view_id)
      })
      .map(view_pb_without_child_views_from_arc)
      .collect::<Vec<_>>();

    Ok(views)
  }

  /// Retrieves all views.
  ///
  /// It is important to note that this will return a flat map of all views,
  /// excluding all child views themselves, as they are all at the same level in this
  /// map.
  ///
  #[tracing::instrument(level = "debug", skip(self))]
  pub async fn get_all_views_pb(&self) -> FlowyResult<Vec<ViewPB>> {
    let lock = self
      .mutex_folder
      .load_full()
      .ok_or_else(folder_not_init_error)?;

    // trash views and other private views should not be accessed
    let folder = lock.read().await;
    let view_ids_should_be_filtered = Self::get_view_ids_should_be_filtered(&folder);

    let all_views = folder.get_all_views();
    let views = all_views
      .into_iter()
      .filter(|view| !view_ids_should_be_filtered.contains(&view.id))
      .map(view_pb_without_child_views_from_arc)
      .collect::<Vec<_>>();

    Ok(views)
  }

  /// Retrieves the ancestors of the view corresponding to the specified view ID, including the view itself.
  ///
  /// For example, if the view hierarchy is as follows:
  ///   - View A
  ///    - View B
  ///     - View C
  ///
  /// If you invoke this method with the ID of View C, it will return a list of views: [View A, View B, View C].
  #[tracing::instrument(level = "debug", skip(self))]
  pub async fn get_view_ancestors_pb(&self, view_id: &str) -> FlowyResult<Vec<ViewPB>> {
    let mut ancestors = vec![];
    let mut parent_view_id = view_id.to_string();
    if let Some(lock) = self.mutex_folder.load_full() {
      let folder = lock.read().await;
      while let Some(view) = folder.get_view(&parent_view_id) {
        // If the view is already in the ancestors list, then break the loop
        if ancestors.iter().any(|v: &ViewPB| v.id == view.id) {
          break;
        }
        ancestors.push(view_pb_without_child_views(view.as_ref().clone()));
        parent_view_id.clone_from(&view.parent_view_id);
      }
      ancestors.reverse();
    }
    Ok(ancestors)
  }

  /// Move the view to trash. If the view is the current view, then set the current view to empty.
  /// When the view is moved to trash, all the child views will be moved to trash as well.
  /// All the favorite views being trashed will be unfavorited first to remove it from favorites list as well. The process of unfavoriting concerned view is handled by `unfavorite_view_and_decendants()`
  #[tracing::instrument(level = "debug", skip(self), err)]
  pub async fn move_view_to_trash(&self, view_id: &str) -> FlowyResult<()> {
    if let Some(lock) = self.mutex_folder.load_full() {
      let mut folder = lock.write().await;
      // Check if the view is already in trash, if not we can move the same
      // view to trash multiple times (duplicates)
      let trash_info = folder.get_my_trash_info();
      if trash_info.into_iter().any(|info| info.id == view_id) {
        return Err(FlowyError::new(
          ErrorCode::Internal,
          format!(
            "Can't move the view({}) to trash, it is already in trash",
            view_id
          ),
        ));
      }

      if let Some(view) = folder.get_view(view_id) {
        // if the view is locked, the view can't be moved to trash
        if view.is_locked.unwrap_or(false) {
          return Err(FlowyError::view_is_locked());
        }

        Self::unfavorite_view_and_decendants(view.clone(), &mut folder);
        folder.add_trash_view_ids(vec![view_id.to_string()]);
        drop(folder);

        // notify the parent view that the view is moved to trash
        folder_notification_builder(view_id, FolderNotification::DidMoveViewToTrash)
          .payload(DeletedViewPB {
            view_id: view_id.to_string(),
            index: None,
          })
          .send();

        notify_child_views_changed(
          view_pb_without_child_views(view.as_ref().clone()),
          ChildViewChangeReason::Delete,
        );
      }
    }

    Ok(())
  }

  fn unfavorite_view_and_decendants(view: Arc<View>, folder: &mut Folder) {
    let mut all_descendant_views: Vec<Arc<View>> = vec![view.clone()];
    all_descendant_views.extend(folder.get_views_belong_to(&view.id));

    let favorite_descendant_views: Vec<ViewPB> = all_descendant_views
      .iter()
      .filter(|view| view.is_favorite)
      .map(|view| view_pb_without_child_views(view.as_ref().clone()))
      .collect();

    if !favorite_descendant_views.is_empty() {
      folder.delete_favorite_view_ids(
        favorite_descendant_views
          .iter()
          .map(|v| v.id.clone())
          .collect(),
      );
      folder_notification_builder("favorite", FolderNotification::DidUnfavoriteView)
        .payload(RepeatedViewPB {
          items: favorite_descendant_views,
        })
        .send();
    }
  }

  /// Moves a nested view to a new location in the hierarchy.
  ///
  /// This function takes the `view_id` of the view to be moved,
  /// `new_parent_id` of the view under which the `view_id` should be moved,
  /// and an optional `prev_view_id` to position the `view_id` right after
  /// this specific view.
  ///
  /// If `prev_view_id` is provided, the moved view will be placed right after
  /// the view corresponding to `prev_view_id` under the `new_parent_id`.
  /// If `prev_view_id` is `None`, the moved view will become the first child of the new parent.
  ///
  /// # Arguments
  ///
  /// * `view_id` - A string slice that holds the id of the view to be moved.
  /// * `new_parent_id` - A string slice that holds the id of the new parent view.
  /// * `prev_view_id` - An `Option<String>` that holds the id of the view after which the `view_id` should be positioned.
  ///
  #[tracing::instrument(level = "trace", skip(self), err)]
  pub async fn move_nested_view(&self, params: MoveNestedViewParams) -> FlowyResult<()> {
    let workspace_id = self.user.workspace_id()?;
    let view_id = params.view_id;
    let new_parent_id = params.new_parent_id;
    let prev_view_id = params.prev_view_id;
    let from_section = params.from_section;
    let to_section = params.to_section;
    let view = self.get_view_pb(&view_id.to_string()).await?;
    // if the view is locked, the view can't be moved
    if view.is_locked.unwrap_or(false) {
      return Err(FlowyError::view_is_locked());
    }

    let old_parent_id = Uuid::from_str(&view.parent_view_id)?;
    if let Some(lock) = self.mutex_folder.load_full() {
      let mut folder = lock.write().await;
      folder.move_nested_view(
        &view_id.to_string(),
        &new_parent_id.to_string(),
        prev_view_id.map(|s| s.to_string()),
      );
      if from_section != to_section {
        if to_section == Some(ViewSectionPB::Private) {
          folder.add_private_view_ids(vec![view_id.to_string()]);
        } else {
          folder.delete_private_view_ids(vec![view_id.to_string()]);
        }
      }
      notify_parent_view_did_change(workspace_id, &folder, vec![new_parent_id, old_parent_id]);
    }
    Ok(())
  }

  /// Move the view with given id from one position to another position.
  /// The view will be moved to the new position in the same parent view.
  /// The passed in index is the index of the view that displayed in the UI.
  /// We need to convert the index to the real index of the view in the parent view.
  #[tracing::instrument(level = "trace", skip(self), err)]
  pub async fn move_view(&self, view_id: &str, from: usize, to: usize) -> FlowyResult<()> {
    let workspace_id = self.user.workspace_id()?;
    let view = self.get_view_pb(view_id).await?;
    // if the view is locked, the view can't be moved
    if view.is_locked.unwrap_or(false) {
      return Err(FlowyError::view_is_locked());
    }

    if let Some((is_workspace, parent_view_id, child_views)) = self.get_view_relation(view_id).await
    {
      // The display parent view is the view that is displayed in the UI
      let display_views = if is_workspace {
        self
          .get_current_workspace()
          .await?
          .views
          .into_iter()
          .map(|view| view.id)
          .collect::<Vec<_>>()
      } else {
        self
          .get_view_pb(&parent_view_id)
          .await?
          .child_views
          .into_iter()
          .map(|view| view.id)
          .collect::<Vec<_>>()
      };

      if display_views.len() > to {
        let to_view_id = display_views[to].clone();

        // Find the actual index of the view in the parent view
        let actual_from_index = child_views.iter().position(|id| id == view_id);
        let actual_to_index = child_views.iter().position(|id| id == &to_view_id);
        if let (Some(actual_from_index), Some(actual_to_index)) =
          (actual_from_index, actual_to_index)
        {
          if let Some(lock) = self.mutex_folder.load_full() {
            let mut folder = lock.write().await;
            folder.move_view(view_id, actual_from_index as u32, actual_to_index as u32);
            let parent_view_id = Uuid::from_str(&parent_view_id)?;
            notify_parent_view_did_change(workspace_id, &folder, vec![parent_view_id]);
          }
        }
      }
    }
    Ok(())
  }

  /// Return a list of views that belong to the given parent view id.
  #[tracing::instrument(level = "debug", skip(self, parent_view_id), err)]
  pub async fn get_views_belong_to(&self, parent_view_id: &str) -> FlowyResult<Vec<Arc<View>>> {
    match self.mutex_folder.load_full() {
      Some(folder) => Ok(folder.read().await.get_views_belong_to(parent_view_id)),
      None => Ok(Vec::default()),
    }
  }

  /// Return a list of views that belong to the given parent view id, and not
  /// in the trash section.
  pub async fn get_untrashed_views_belong_to(
    &self,
    parent_view_id: &str,
  ) -> FlowyResult<Vec<Arc<View>>> {
    match self.mutex_folder.load_full() {
      Some(folder) => {
        let folder = folder.read().await;
        let views = folder
          .get_views_belong_to(parent_view_id)
          .into_iter()
          .filter(|view| !folder.is_view_in_section(Section::Trash, &view.id))
          .collect();

        Ok(views)
      },
      None => Ok(vec![]),
    }
  }

  pub async fn get_view(&self, view_id: &str) -> FlowyResult<Arc<View>> {
    match self.mutex_folder.load_full() {
      Some(folder) => {
        let folder = folder.read().await;
        Ok(
          folder
            .get_view(view_id)
            .ok_or_else(FlowyError::record_not_found)?,
        )
      },
      None => Err(FlowyError::internal().with_context("The folder is not initialized")),
    }
  }

  /// Update the view with the given params.
  #[tracing::instrument(level = "trace", skip(self), err)]
  pub async fn update_view_with_params(&self, params: UpdateViewParams) -> FlowyResult<()> {
    self
      .update_view(&params.view_id, true, |update| {
        update
          .set_name_if_not_none(params.name)
          .set_desc_if_not_none(params.desc)
          .set_layout_if_not_none(params.layout)
          .set_favorite_if_not_none(params.is_favorite)
          .set_extra_if_not_none(params.extra)
          .done()
      })
      .await
  }

  /// Update the icon of the view with the given params.
  #[tracing::instrument(level = "trace", skip(self), err)]
  pub async fn update_view_icon_with_params(
    &self,
    params: UpdateViewIconParams,
  ) -> FlowyResult<()> {
    self
      .update_view(&params.view_id, true, |update| {
        update.set_icon(params.icon).done()
      })
      .await
  }

  /// Lock the view with the given view id.
  ///
  /// If the view is locked, it cannot be edited.
  #[tracing::instrument(level = "debug", skip(self), err)]
  pub async fn lock_view(&self, view_id: &str) -> FlowyResult<()> {
    self
      .update_view(view_id, false, |update| {
        update.set_page_lock_status(true).done()
      })
      .await
  }

  /// Unlock the view with the given view id.
  #[tracing::instrument(level = "debug", skip(self), err)]
  pub async fn unlock_view(&self, view_id: &str) -> FlowyResult<()> {
    self
      .update_view(view_id, false, |update| {
        update.set_page_lock_status(false).done()
      })
      .await
  }

  /// Duplicate the view with the given view id.
  ///
  /// Including the view data (icon, cover, extra) and the child views.
  #[tracing::instrument(level = "debug", skip(self), err)]
  pub(crate) async fn duplicate_view(
    &self,
    params: DuplicateViewParams,
  ) -> Result<ViewPB, FlowyError> {
    let lock = self
      .mutex_folder
      .load_full()
      .ok_or_else(|| FlowyError::record_not_found().with_context("Can't duplicate the view"))?;
    let folder = lock.read().await;
    let view = folder
      .get_view(&params.view_id)
      .ok_or_else(|| FlowyError::record_not_found().with_context("Can't duplicate the view"))?;

    // Explicitly drop the folder lock to avoid deadlock when following calls contains 'self'
    drop(folder);

    let parent_view_id = params
      .parent_view_id
      .clone()
      .unwrap_or(view.parent_view_id.clone());
    self
      .duplicate_view_with_parent_id(
        &view.id,
        &parent_view_id,
        params.open_after_duplicate,
        params.include_children,
        params.suffix,
        params.sync_after_create,
      )
      .await
  }

  /// Duplicate the view with the given view id and parent view id.
  ///
  /// If the view id is the same as the parent view id, it will return an error.
  /// If the view id is not found, it will return an error.
  pub(crate) async fn duplicate_view_with_parent_id(
    &self,
    view_id: &str,
    parent_view_id: &str,
    open_after_duplicated: bool,
    include_children: bool,
    suffix: Option<String>,
    sync_after_create: bool,
  ) -> Result<ViewPB, FlowyError> {
    if view_id == parent_view_id {
      return Err(FlowyError::new(
        ErrorCode::Internal,
        format!("Can't duplicate the view({}) to itself", view_id),
      ));
    }

    // filter the view ids that in the trash or private section
    let filtered_view_ids = match self.mutex_folder.load_full() {
      None => Vec::default(),
      Some(lock) => {
        let folder = lock.read().await;
        Self::get_view_ids_should_be_filtered(&folder)
      },
    };

    // only apply the `open_after_duplicated` and the `include_children` to the first view
    let mut is_source_view = true;
    let mut new_view_id = String::default();
    // use a stack to duplicate the view and its children
    let mut stack = vec![(view_id.to_string(), parent_view_id.to_string())];
    let mut objects = vec![];
    let suffix = suffix.unwrap_or(" (copy)".to_string());

    let lock = match self.mutex_folder.load_full() {
      None => {
        return Err(
          FlowyError::record_not_found()
            .with_context(format!("Can't duplicate the view({})", view_id)),
        );
      },
      Some(lock) => lock,
    };
    while let Some((current_view_id, current_parent_id)) = stack.pop() {
      let view = lock
        .read()
        .await
        .get_view(&current_view_id)
        .ok_or_else(|| {
          FlowyError::record_not_found()
            .with_context(format!("Can't duplicate the view({})", view_id))
        })?;

      let handler = self.get_handler(&view.layout)?;
      info!(
        "{} duplicate view{}, name:{}, layout:{:?}",
        handler.name(),
        view.id,
        view.name,
        view.layout
      );
      let view_id = Uuid::from_str(&view.id)?;
      let view_data = handler.duplicate_view(&view_id).await?;

      let index = self
        .get_view_relation(&current_parent_id)
        .await
        .and_then(|(_, _, views)| {
          views
            .iter()
            .filter(|id| filtered_view_ids.contains(id))
            .position(|id| *id == current_view_id)
            .map(|i| i as u32)
        });

      let section = {
        let folder = lock.read().await;
        if folder.is_view_in_section(Section::Private, &view.id) {
          ViewSectionPB::Private
        } else {
          ViewSectionPB::Public
        }
      };

      let name = if is_source_view {
        format!(
          "{}{}",
          if view.name.is_empty() {
            "Untitled"
          } else {
            view.name.as_str()
          },
          suffix
        )
      } else {
        view.name.clone()
      };

      let parent_view_id = Uuid::from_str(&current_parent_id)?;
      let duplicate_params = CreateViewParams {
        parent_view_id,
        name,
        layout: view.layout.clone().into(),
        initial_data: ViewData::DuplicateData(view_data),
        view_id: gen_view_id(),
        meta: Default::default(),
        set_as_current: is_source_view && open_after_duplicated,
        index,
        section: Some(section),
        extra: view.extra.clone(),
        icon: view.icon.clone(),
      };

      // set the notify_workspace_update to false to avoid multiple notifications
      let (duplicated_view, encoded_collab) = self
        .create_view_with_params(duplicate_params, false)
        .await?;

      if is_source_view {
        new_view_id.clone_from(&duplicated_view.id);
      }

      if sync_after_create {
        if let Some(encoded_collab) = encoded_collab {
          let object_id = Uuid::from_str(&duplicated_view.id)?;
          let collab_type = match duplicated_view.layout {
            ViewLayout::Document => CollabType::Document,
            ViewLayout::Board | ViewLayout::Grid | ViewLayout::Calendar => CollabType::Database,
            ViewLayout::Chat => CollabType::Unknown,
          };
          // don't block the whole import process if the view can't be encoded
          if collab_type != CollabType::Unknown {
            match self.get_folder_collab_params(object_id, collab_type, encoded_collab) {
              Ok(params) => objects.push(params),
              Err(e) => {
                error!("duplicate error {}", e);
              },
            }
          }
        }
      }

      if include_children {
        let child_views = self.get_views_belong_to(&current_view_id).await?;
        // reverse the child views to keep the order
        for child_view in child_views.iter().rev() {
          // skip the view_id should be filtered and the child_view is the duplicated view
          if !filtered_view_ids.contains(&child_view.id) && child_view.layout != ViewLayout::Chat {
            stack.push((child_view.id.clone(), duplicated_view.id.clone()));
          }
        }
      }

      is_source_view = false
    }

    let workspace_id = self.user.workspace_id()?;
    let parent_view_id = Uuid::from_str(parent_view_id)?;

    // Sync the view to the cloud
    if sync_after_create {
      self
        .cloud_service()?
        .batch_create_folder_collab_objects(&workspace_id, objects)
        .await?;
    }

    // notify the update here
    let folder = lock.read().await;
    notify_parent_view_did_change(workspace_id, &folder, vec![parent_view_id]);
    let duplicated_view = self.get_view_pb(&new_view_id).await?;

    Ok(duplicated_view)
  }

  #[tracing::instrument(level = "trace", skip(self), err)]
  pub(crate) async fn set_current_view(&self, view_id: String) -> Result<(), FlowyError> {
    if let Some(lock) = self.mutex_folder.load_full() {
      let mut folder = lock.write().await;
      folder.set_current_view(view_id.clone());
      folder.add_recent_view_ids(vec![view_id.clone()]);
    } else {
      return Err(FlowyError::record_not_found());
    }

    let view = self.get_current_view().await;
    if let Some(view) = &view {
      let view_layout: ViewLayout = view.layout.clone().into();
      if let Some(handle) = self.operation_handlers.get(&view_layout) {
        info!("Open view: {}-{}", view.name, view.id);
        let view_id = Uuid::from_str(&view.id)?;
        if let Err(err) = handle.open_view(&view_id).await {
          error!("Open view error: {:?}", err);
        }
      }
    }

    let workspace_id = self.user.workspace_id()?;
    let setting = WorkspaceLatestPB {
      workspace_id: workspace_id.to_string(),
      latest_view: view,
    };
    folder_notification_builder(workspace_id, FolderNotification::DidUpdateWorkspaceSetting)
      .payload(setting)
      .send();
    Ok(())
  }

  #[tracing::instrument(level = "trace", skip(self))]
  pub(crate) async fn get_current_view(&self) -> Option<ViewPB> {
    let view_id = {
      let lock = self.mutex_folder.load_full()?;
      let folder = lock.read().await;
      let view = folder.get_current_view()?;
      drop(folder);
      view
    };
    self.get_view_pb(&view_id).await.ok()
  }

  /// Toggles the favorite status of a view identified by `view_id`If the view is not a favorite, it will be added to the favorites list; otherwise, it will be removed from the list.
  #[tracing::instrument(level = "debug", skip(self), err)]
  pub async fn toggle_favorites(&self, view_id: &str) -> FlowyResult<()> {
    if let Some(lock) = self.mutex_folder.load_full() {
      let mut folder = lock.write().await;
      if let Some(old_view) = folder.get_view(view_id) {
        if old_view.is_favorite {
          folder.delete_favorite_view_ids(vec![view_id.to_string()]);
        } else {
          folder.add_favorite_view_ids(vec![view_id.to_string()]);
        }
      }
    }
    self.send_toggle_favorite_notification(view_id).await;
    Ok(())
  }

  /// Add the view to the recent view list / history.
  #[tracing::instrument(level = "debug", skip(self), err)]
  pub async fn add_recent_views(&self, view_ids: Vec<String>) -> FlowyResult<()> {
    if let Some(lock) = self.mutex_folder.load_full() {
      let mut folder = lock.write().await;
      folder.add_recent_view_ids(view_ids);
    }
    self.send_update_recent_views_notification().await;
    Ok(())
  }

  /// Add the view to the recent view list / history.
  #[tracing::instrument(level = "debug", skip(self), err)]
  pub async fn remove_recent_views(&self, view_ids: Vec<String>) -> FlowyResult<()> {
    if let Some(lock) = self.mutex_folder.load_full() {
      let mut folder = lock.write().await;
      folder.delete_recent_view_ids(view_ids);
    }
    self.send_update_recent_views_notification().await;
    Ok(())
  }

  /// Share the page with a user (member or guest).
  pub async fn share_page_with_user(
    &self,
    params: ShareViewWithGuestRequest,
  ) -> Result<(), FlowyError> {
    let workspace_id = self.user.workspace_id()?;
    self
      .cloud_service()?
      .share_page_with_user(&workspace_id, params)
      .await?;
    Ok(())
  }

  /// Revoke the shared page access of a user (member or guest).
  pub async fn revoke_shared_page_access(
    &self,
    page_id: &Uuid,
    params: RevokeSharedViewAccessRequest,
  ) -> Result<(), FlowyError> {
    let workspace_id = self.user.workspace_id()?;
    self
      .cloud_service()?
      .revoke_shared_page_access(&workspace_id, page_id, params)
      .await?;
    Ok(())
  }

  /// Get the shared page details.
  pub async fn get_shared_page_details(
    &self,
    page_id: &Uuid,
  ) -> Result<SharedViewDetails, FlowyError> {
    let workspace_id = self.user.workspace_id()?;
    let result = self
      .cloud_service()?
      .get_shared_page_details(&workspace_id, page_id)
      .await?;
    Ok(result)
  }

  /// Publishes a view identified by the given `view_id`.
  ///
  /// If `publish_name` is `None`, a default name will be generated using the view name and view id.
  #[tracing::instrument(level = "debug", skip(self), err)]
  pub async fn publish_view(
    &self,
    view_id: &str,
    publish_name: Option<String>,
    selected_view_ids: Option<Vec<String>>,
  ) -> FlowyResult<()> {
    let view = {
      let lock = match self.mutex_folder.load_full() {
        None => {
          return Err(
            FlowyError::record_not_found()
              .with_context(format!("Can't find the view with ID: {}", view_id)),
          );
        },
        Some(lock) => lock,
      };
      let read_guard = lock.read().await;
      read_guard.get_view(view_id).ok_or_else(|| {
        FlowyError::record_not_found()
          .with_context(format!("Can't find the view with ID: {}", view_id))
      })?
    };

    if view.layout == ViewLayout::Chat {
      return Err(FlowyError::new(
        ErrorCode::NotSupportYet,
        "The chat view is not supported to publish.".to_string(),
      ));
    }

    // Retrieve the view payload and its child views recursively
    let payload = self
      .get_batch_publish_payload(view_id, publish_name, false)
      .await?;

    // set the selected view ids to the payload
    let payload = if let Some(selected_view_ids) = selected_view_ids {
      payload
        .into_iter()
        .map(|mut p| {
          if let PublishPayload::Database(p) = &mut p {
            p.data
              .visible_database_view_ids
              .clone_from(&selected_view_ids);
          }
          p
        })
        .collect::<Vec<_>>()
    } else {
      payload
    };

    let workspace_id = self.user.workspace_id()?;
    self
      .cloud_service()?
      .publish_view(&workspace_id, payload)
      .await?;
    Ok(())
  }

  /// Unpublish the view with the given view id.
  #[tracing::instrument(level = "debug", skip(self), err)]
  pub async fn unpublish_views(&self, view_ids: Vec<Uuid>) -> FlowyResult<()> {
    let workspace_id = self.user.workspace_id()?;
    self
      .cloud_service()?
      .unpublish_views(&workspace_id, view_ids)
      .await?;
    Ok(())
  }

  /// Get the publish info of the view with the given view id.
  /// The publish info contains the namespace and publish_name of the view.
  #[tracing::instrument(level = "debug", skip(self))]
  pub async fn get_publish_info(&self, view_id: &Uuid) -> FlowyResult<PublishInfo> {
    let publish_info = self.cloud_service()?.get_publish_info(view_id).await?;
    Ok(publish_info)
  }

  /// Sets the publish name of the view with the given view id.
  #[tracing::instrument(level = "debug", skip(self))]
  pub async fn set_publish_name(&self, view_id: Uuid, new_name: String) -> FlowyResult<()> {
    let workspace_id = self.user.workspace_id()?;
    self
      .cloud_service()?
      .set_publish_name(&workspace_id, view_id, new_name)
      .await?;
    Ok(())
  }

  /// Get the namespace of the current workspace.
  /// The namespace is used to generate the URL of the published view.
  #[tracing::instrument(level = "debug", skip(self), err)]
  pub async fn set_publish_namespace(&self, new_namespace: String) -> FlowyResult<()> {
    let workspace_id = self.user.workspace_id()?;
    self
      .cloud_service()?
      .set_publish_namespace(&workspace_id, new_namespace)
      .await?;
    Ok(())
  }

  /// Get the namespace of the current workspace.
  #[tracing::instrument(level = "debug", skip(self), err)]
  pub async fn get_publish_namespace(&self) -> FlowyResult<String> {
    let workspace_id = self.user.workspace_id()?;
    let namespace = self
      .cloud_service()?
      .get_publish_namespace(&workspace_id)
      .await?;
    Ok(namespace)
  }

  /// List all published views of the current workspace.
  #[tracing::instrument(level = "debug", skip(self), err)]
  pub async fn list_published_views(&self) -> FlowyResult<Vec<PublishInfoView>> {
    let workspace_id = self.user.workspace_id()?;
    let published_views = self
      .cloud_service()?
      .list_published_views(&workspace_id)
      .await?;
    Ok(published_views)
  }

  #[tracing::instrument(level = "debug", skip(self), err)]
  pub async fn get_default_published_view_info(&self) -> FlowyResult<PublishInfo> {
    let workspace_id = self.user.workspace_id()?;
    let default_published_view_info = self
      .cloud_service()?
      .get_default_published_view_info(&workspace_id)
      .await?;
    Ok(default_published_view_info)
  }

  #[tracing::instrument(level = "debug", skip(self), err)]
  pub async fn set_default_published_view(&self, view_id: uuid::Uuid) -> FlowyResult<()> {
    let workspace_id = self.user.workspace_id()?;
    self
      .cloud_service()?
      .set_default_published_view(&workspace_id, view_id)
      .await?;
    Ok(())
  }

  #[tracing::instrument(level = "debug", skip(self), err)]
  pub async fn remove_default_published_view(&self) -> FlowyResult<()> {
    let workspace_id = self.user.workspace_id()?;
    self
      .cloud_service()?
      .remove_default_published_view(&workspace_id)
      .await?;
    Ok(())
  }

  /// Retrieves the publishing payload for a specified view and optionally its child views.
  ///
  /// # Arguments
  /// * `view_id` - The ID of the view to publish.
  /// * `publish_name` - Optional name for the published view.
  /// * `include_children` - Flag to include child views in the payload.
  pub async fn get_batch_publish_payload(
    &self,
    view_id: &str,
    publish_name: Option<String>,
    include_children: bool,
  ) -> FlowyResult<Vec<PublishPayload>> {
    let mut stack = vec![view_id.to_string()];
    let mut payloads = Vec::new();

    while let Some(current_view_id) = stack.pop() {
      let view = match self.get_view_pb(&current_view_id).await {
        Ok(view) => view,
        Err(_) => continue,
      };

      // Skip the chat view
      if view.layout == ViewLayoutPB::Chat {
        continue;
      }

      let layout: ViewLayout = view.layout.into();

      // Only support set the publish_name for the current view, not for the child views
      let publish_name = if current_view_id == view_id {
        publish_name.clone()
      } else {
        None
      };

      if let Ok(payload) = self
        .get_publish_payload(&Uuid::from_str(&current_view_id)?, publish_name, layout)
        .await
      {
        payloads.push(payload);
      }

      if include_children {
        // Add the child views to the stack
        stack.extend(view.child_views.iter().map(|child| child.id.clone()));
      }
    }

    Ok(payloads)
  }

  async fn build_publish_views(&self, view_id: &str) -> Option<PublishViewInfo> {
    let view_pb = self.get_view_pb(view_id).await.ok()?;

    let mut child_views_futures = vec![];

    for child in &view_pb.child_views {
      let future = self.build_publish_views(&child.id);
      child_views_futures.push(future);
    }

    let child_views = future::join_all(child_views_futures)
      .await
      .into_iter()
      .flatten()
      .collect::<Vec<PublishViewInfo>>();

    let view_child_views = if child_views.is_empty() {
      None
    } else {
      Some(child_views)
    };

    let view = view_pb_to_publish_view(&view_pb);

    let view = PublishViewInfo {
      child_views: view_child_views,
      ..view
    };

    Some(view)
  }

  async fn get_publish_payload(
    &self,
    view_id: &Uuid,
    publish_name: Option<String>,
    layout: ViewLayout,
  ) -> FlowyResult<PublishPayload> {
    let handler = self.get_handler(&layout)?;
    let encoded_collab_wrapper: GatherEncodedCollab = handler
      .gather_publish_encode_collab(&self.user, view_id)
      .await?;

    let view_str_id = view_id.to_string();
    let view = self.get_view_pb(&view_str_id).await?;

    let publish_name = publish_name.unwrap_or_else(|| generate_publish_name(&view.id, &view.name));

    let child_views = self
      .build_publish_views(&view_str_id)
      .await
      .and_then(|v| v.child_views)
      .unwrap_or_default();

    let ancestor_views = self
      .get_view_ancestors_pb(&view_str_id)
      .await?
      .iter()
      .map(view_pb_to_publish_view)
      .collect::<Vec<PublishViewInfo>>();

    let metadata = PublishViewMetaData {
      view: view_pb_to_publish_view(&view),
      child_views,
      ancestor_views,
    };
    let meta = PublishViewMeta {
      view_id: view.id.clone(),
      publish_name,
      metadata,
    };

    let payload = match encoded_collab_wrapper {
      GatherEncodedCollab::Database(v) => {
        let database_collab = v.database_encoded_collab.doc_state.to_vec();
        let database_relations = v.database_relations;
        let database_row_collabs = v
        .database_row_encoded_collabs
        .into_iter()
        .map(|v| (v.0, v.1.doc_state.to_vec())) // Convert to HashMap
        .collect::<HashMap<String, Vec<u8>>>();
        let database_row_document_collabs = v
          .database_row_document_encoded_collabs
          .into_iter()
          .map(|v| (v.0, v.1.doc_state.to_vec())) // Convert to HashMap
          .collect::<HashMap<String, Vec<u8>>>();

        let data = PublishDatabaseData {
          database_collab,
          database_row_collabs,
          database_relations,
          database_row_document_collabs,
          ..Default::default()
        };
        PublishPayload::Database(PublishDatabasePayload { meta, data })
      },
      GatherEncodedCollab::Document(v) => {
        let data = v.doc_state.to_vec();
        PublishPayload::Document(PublishDocumentPayload { meta, data })
      },
      GatherEncodedCollab::Unknown => PublishPayload::Unknown,
    };

    Ok(payload)
  }

  // Used by toggle_favorites to send notification to frontend, after the favorite status of view has been changed.It sends two distinct notifications: one to correctly update the concerned view's is_favorite status, and another to update the list of favorites that is to be displayed.
  async fn send_toggle_favorite_notification(&self, view_id: &str) {
    if let Ok(view) = self.get_view_pb(view_id).await {
      let notification_type = if view.is_favorite {
        FolderNotification::DidFavoriteView
      } else {
        FolderNotification::DidUnfavoriteView
      };
      folder_notification_builder("favorite", notification_type)
        .payload(RepeatedViewPB {
          items: vec![view.clone()],
        })
        .send();

      folder_notification_builder(&view.id, FolderNotification::DidUpdateView)
        .payload(view)
        .send()
    }
  }

  async fn send_update_recent_views_notification(&self) {
    let recent_views = self.get_my_recent_sections().await;
    folder_notification_builder("recent_views", FolderNotification::DidUpdateRecentViews)
      .payload(RepeatedViewIdPB {
        items: recent_views.into_iter().map(|item| item.id).collect(),
      })
      .send();
  }

  #[tracing::instrument(level = "trace", skip(self))]
  pub(crate) async fn get_all_favorites(&self) -> Vec<SectionItem> {
    self.get_sections(Section::Favorite).await
  }

  pub async fn get_all_views(&self) -> FlowyResult<Vec<Arc<View>>> {
    let lock = self
      .mutex_folder
      .load_full()
      .ok_or_else(folder_not_init_error)?;
    let views = lock
      .read()
      .await
      .get_all_views()
      .into_iter()
      .collect::<Vec<_>>();
    Ok(views)
  }

  #[tracing::instrument(level = "debug", skip(self))]
  pub(crate) async fn get_my_recent_sections(&self) -> Vec<SectionItem> {
    self.get_sections(Section::Recent).await
  }

  #[tracing::instrument(level = "trace", skip(self))]
  pub(crate) async fn get_my_trash_info(&self) -> Vec<TrashInfo> {
    match self.mutex_folder.load_full() {
      None => Vec::default(),
      Some(folder) => folder.read().await.get_my_trash_info(),
    }
  }

  #[tracing::instrument(level = "trace", skip(self))]
  pub(crate) async fn restore_all_trash(&self) {
    if let Some(lock) = self.mutex_folder.load_full() {
      let mut folder = lock.write().await;
      folder.remove_all_my_trash_sections();
      folder_notification_builder("trash", FolderNotification::DidUpdateTrash)
        .payload(RepeatedTrashPB { items: vec![] })
        .send();
    }
  }

  #[tracing::instrument(level = "trace", skip(self))]
  pub(crate) async fn restore_trash(&self, trash_id: &str) {
    if let Some(lock) = self.mutex_folder.load_full() {
      let mut folder = lock.write().await;
      folder.delete_trash_view_ids(vec![trash_id.to_string()]);
    }
  }

  /// Delete all the trash permanently.
  #[tracing::instrument(level = "trace", skip(self))]
  pub(crate) async fn delete_my_trash(&self) {
    if let Some(lock) = self.mutex_folder.load_full() {
      let deleted_trash = lock.read().await.get_my_trash_info();

      // Explicitly drop the folder lock to avoid deadlock when following calls contains 'self'
      drop(lock);

      for trash in deleted_trash {
        let _ = self.delete_trash(&trash.id).await;
      }
      folder_notification_builder("trash", FolderNotification::DidUpdateTrash)
        .payload(RepeatedTrashPB { items: vec![] })
        .send();
    }
  }

  /// Delete the trash permanently.
  /// Delete the view will delete all the resources that the view holds. For example, if the view
  /// is a database view. Then the database will be deleted as well.
  #[tracing::instrument(level = "debug", skip(self, view_id), err)]
  pub async fn delete_trash(&self, view_id: &str) -> FlowyResult<()> {
    if let Some(lock) = self.mutex_folder.load_full() {
      let view = {
        let mut folder = lock.write().await;
        let view = folder.get_view(view_id);
        folder.delete_trash_view_ids(vec![view_id.to_string()]);
        folder.delete_views(vec![view_id]);
        view
      };

      if let Some(view) = view {
        let view_id = Uuid::from_str(view_id)?;
        if let Ok(handler) = self.get_handler(&view.layout) {
          handler.delete_view(&view_id).await?;
        }
      }
    }
    Ok(())
  }

  /// Imports a single file to the folder and returns the encoded collab for immediate cloud sync.
  #[allow(clippy::type_complexity)]
  #[instrument(level = "debug", skip_all, err)]
  pub(crate) async fn import_single_file(
    &self,
    parent_view_id: Uuid,
    import_data: ImportItem,
  ) -> FlowyResult<(View, Vec<(String, CollabType, EncodedCollab)>)> {
    let handler = self.get_handler(&import_data.view_layout)?;
    let view_id = gen_view_id();
    let uid = self.user.user_id()?;
    let mut encoded_collab = vec![];

    info!("import single file from:{}", import_data.data);
    match import_data.data {
      ImportData::FilePath { file_path } => {
        handler
          .import_from_file_path(&view_id.to_string(), &import_data.name, file_path)
          .await?;
      },
      ImportData::Bytes { bytes } => {
        encoded_collab = handler
          .import_from_bytes(
            uid,
            &view_id,
            &import_data.name,
            import_data.import_type,
            bytes,
          )
          .await?;
      },
    }

    let params = CreateViewParams {
      parent_view_id,
      name: import_data.name,
      layout: import_data.view_layout.clone().into(),
      initial_data: ViewData::Empty,
      view_id,
      meta: Default::default(),
      set_as_current: false,
      index: None,
      section: None,
      extra: None,
      icon: None,
    };

    let view = create_view(self.user.user_id()?, params, import_data.view_layout);

    // Insert the new view into the folder
    if let Some(lock) = self.mutex_folder.load_full() {
      let mut folder = lock.write().await;
      folder.insert_view(view.clone(), None);
    }

    Ok((view, encoded_collab))
  }

  pub(crate) async fn import_zip_file(&self, zip_file_path: &str) -> FlowyResult<()> {
    self.cloud_service()?.import_zip(zip_file_path).await?;
    Ok(())
  }

  /// Import function to handle the import of data.
  pub(crate) async fn import(&self, import_data: ImportParams) -> FlowyResult<RepeatedViewPB> {
    let workspace_id = self.user.workspace_id()?;
    let mut objects = vec![];
    let mut views = vec![];
    for data in import_data.items {
      // Import a single file and get the view and encoded collab data
      let (view, encoded_collabs) = self
        .import_single_file(import_data.parent_view_id, data)
        .await?;
      views.push(view_pb_without_child_views(view));

      for (object_id, collab_type, encode_collab) in encoded_collabs {
        if let Ok(object_id) = Uuid::from_str(&object_id) {
          match self.get_folder_collab_params(object_id, collab_type, encode_collab) {
            Ok(params) => objects.push(params),
            Err(e) => {
              error!("import error {}", e);
            },
          }
        }
      }
    }

    info!("Syncing the imported {} collab to the cloud", objects.len());
    self
      .cloud_service()?
      .batch_create_folder_collab_objects(&workspace_id, objects)
      .await?;

    // Notify that the parent view has changed
    if let Some(lock) = self.mutex_folder.load_full() {
      let folder = lock.read().await;
      notify_parent_view_did_change(workspace_id, &folder, vec![import_data.parent_view_id]);
    }

    Ok(RepeatedViewPB { items: views })
  }

  /// Update the view with the provided view_id using the specified function.
  ///
  /// If the check_locked is true, it will check the lock status of the view. If the view is locked,
  /// it will return an error.
  async fn update_view<F>(&self, view_id: &str, check_locked: bool, f: F) -> FlowyResult<()>
  where
    F: FnOnce(ViewUpdate) -> Option<View>,
  {
    let workspace_id = self.user.workspace_id()?;
    let value = match self.mutex_folder.load_full() {
      None => None,
      Some(lock) => {
        let mut folder = lock.write().await;
        let old_view = folder.get_view(view_id);

        // Check if the view is locked
        if check_locked && old_view.as_ref().and_then(|v| v.is_locked).unwrap_or(false) {
          return Err(FlowyError::view_is_locked());
        }

        let new_view = folder.update_view(view_id, f);

        Some((old_view, new_view))
      },
    };

    if let Some((Some(old_view), Some(new_view))) = value {
      if let Ok(handler) = self.get_handler(&old_view.layout) {
        handler.did_update_view(&old_view, &new_view).await?;
      }
    }

    if let Ok(view_pb) = self.get_view_pb(view_id).await {
      folder_notification_builder(&view_pb.id, FolderNotification::DidUpdateView)
        .payload(view_pb)
        .send();

      if let Some(lock) = self.mutex_folder.load_full() {
        let folder = lock.read().await;
        notify_did_update_workspace(&workspace_id, &folder);
      }
    }

    Ok(())
  }

  /// Returns a handler that implements the [FolderOperationHandler] trait
  fn get_handler(&self, view_layout: &ViewLayout) -> FlowyResult<Arc<dyn FolderOperationHandler>> {
    match self.operation_handlers.get(view_layout) {
      None => Err(FlowyError::internal().with_context(format!(
        "Get data processor failed. Unknown layout type: {:?}",
        view_layout
      ))),
      Some(processor) => Ok(processor.clone()),
    }
  }

  fn get_folder_collab_params(
    &self,
    object_id: Uuid,
    collab_type: CollabType,
    encoded_collab: EncodedCollab,
  ) -> FlowyResult<FolderCollabParams> {
    // Try to encode the collaboration data to bytes
    let encoded_collab_v1: Result<Vec<u8>, FlowyError> =
      encoded_collab.encode_to_bytes().map_err(internal_error);
    encoded_collab_v1.map(|encoded_collab_v1| FolderCollabParams {
      object_id,
      encoded_collab_v1,
      collab_type,
    })
  }

  /// Returns the relation of the view. The relation is a tuple of (is_workspace, parent_view_id,
  /// child_view_ids). If the view is a workspace, then the parent_view_id is the workspace id.
  /// Otherwise, the parent_view_id is the parent view id of the view. The child_view_ids is the
  /// child view ids of the view.
  async fn get_view_relation(&self, view_id: &str) -> Option<(bool, String, Vec<String>)> {
    let workspace_id = self.user.workspace_id().ok()?;
    let lock = self.mutex_folder.load_full()?;
    let folder = lock.read().await;
    let view = folder.get_view(view_id)?;
    match folder.get_view(&view.parent_view_id) {
      None => folder
        .get_workspace_info(&workspace_id.to_string())
        .map(|workspace| {
          (
            true,
            workspace.id,
            workspace
              .child_views
              .items
              .into_iter()
              .map(|view| view.id)
              .collect::<Vec<String>>(),
          )
        }),
      Some(parent_view) => Some((
        false,
        parent_view.id.clone(),
        parent_view
          .children
          .items
          .clone()
          .into_iter()
          .map(|view| view.id)
          .collect::<Vec<String>>(),
      )),
    }
  }

  pub async fn get_folder_snapshots(
    &self,
    workspace_id: &str,
    limit: usize,
  ) -> FlowyResult<Vec<FolderSnapshotPB>> {
    let snapshots = self
      .cloud_service()?
      .get_folder_snapshots(workspace_id, limit)
      .await?
      .into_iter()
      .map(|snapshot| FolderSnapshotPB {
        snapshot_id: snapshot.snapshot_id,
        snapshot_desc: "".to_string(),
        created_at: snapshot.created_at,
        data: snapshot.data,
      })
      .collect::<Vec<_>>();

    Ok(snapshots)
  }

  pub async fn set_views_visibility(&self, view_ids: Vec<String>, is_public: bool) {
    if let Some(lock) = self.mutex_folder.load_full() {
      let mut folder = lock.write().await;
      if is_public {
        folder.delete_private_view_ids(view_ids);
      } else {
        folder.add_private_view_ids(view_ids);
      }
    }
  }

  /// Only support getting the Favorite and Recent sections.
  async fn get_sections(&self, section_type: Section) -> Vec<SectionItem> {
    match self.mutex_folder.load_full() {
      None => Vec::default(),
      Some(lock) => {
        let folder = lock.read().await;
        let views = match section_type {
          Section::Favorite => folder.get_my_favorite_sections(),
          Section::Recent => folder.get_my_recent_sections(),
          _ => vec![],
        };
        let view_ids_should_be_filtered = Self::get_view_ids_should_be_filtered(&folder);
        views
          .into_iter()
          .filter(|view| !view_ids_should_be_filtered.contains(&view.id))
          .collect()
      },
    }
  }

  /// Get all the view that are in the trash, including the child views of the child views.
  /// For example, if A view which is in the trash has a child view B, this function will return
  /// both A and B.
  fn get_all_trash_ids(folder: &Folder) -> Vec<String> {
    let trash_ids = folder
      .get_all_trash_sections()
      .into_iter()
      .map(|trash| trash.id)
      .collect::<Vec<String>>();
    let mut all_trash_ids = trash_ids.clone();
    for trash_id in trash_ids {
      all_trash_ids.extend(get_all_child_view_ids(folder, &trash_id));
    }
    all_trash_ids
  }

  /// Filter the views that are in the trash and belong to the other private sections.
  fn get_view_ids_should_be_filtered(folder: &Folder) -> Vec<String> {
    let trash_ids = Self::get_all_trash_ids(folder);
    let other_private_view_ids = Self::get_other_private_view_ids(folder);
    [trash_ids, other_private_view_ids].concat()
  }

  fn get_other_private_view_ids(folder: &Folder) -> Vec<String> {
    let my_private_view_ids = folder
      .get_my_private_sections()
      .into_iter()
      .map(|view| view.id)
      .collect::<Vec<String>>();

    let all_private_view_ids = folder
      .get_all_private_sections()
      .into_iter()
      .map(|view| view.id)
      .collect::<Vec<String>>();

    all_private_view_ids
      .into_iter()
      .filter(|id| !my_private_view_ids.contains(id))
      .collect()
  }

  /// Get the shared views of the workspace.
  ///
  /// This function will return the first level of the shared views. If the shared view has child
  /// views, this function will not return the child views.
  pub async fn get_shared_pages(&self) -> FlowyResult<RepeatedSharedViewResponsePB> {
    let uid = self.user.user_id()?;
    let conn = self.user.sqlite_connection(uid)?;
    let workspace_id = self.user.workspace_id()?;
    let mut local_shared_views = vec![];

    let all_views: Vec<Arc<View>> = self.get_all_views().await?;
    let lock = self
      .mutex_folder
      .load_full()
      .ok_or_else(folder_not_init_error)?;
    let folder = lock.read().await;
    // filter the views that are in the trash
    let trash_ids = Self::get_all_trash_ids(&folder);
    let all_views = all_views
      .into_iter()
      .filter(|view| !trash_ids.contains(&view.id))
      .collect::<Vec<Arc<View>>>();

    // 1. Get the data from the local database first
    if let Ok(shared_views) =
      select_all_workspace_shared_views(conn, &workspace_id.to_string(), uid)
    {
      local_shared_views = shared_views
        .into_iter()
        .filter_map(|shared_view| {
          let view = all_views
            .iter()
            .find(|view| view.id == shared_view.view_id)?;
          Some(SharedViewPB {
            view: view_pb_with_all_child_views(view.clone(), &|parent_id| {
              all_views
                .iter()
                .filter(|v| v.parent_view_id == *parent_id)
                .cloned()
                .collect()
            }),
            access_level: AFAccessLevelPB::from(shared_view.permission_id),
          })
        })
        .collect();
    }

    // 2. Fetch the data from the cloud service and persist to the local database
    let cloud_workspace_id = workspace_id;
    let user = self.user.clone();
    let cloud_service = self.cloud_service.clone();
    tokio::spawn(async move {
      if let Some(cloud_service) = cloud_service.upgrade() {
        if let Ok(resp) = cloud_service.get_shared_views(&cloud_workspace_id).await {
          if let Ok(mut conn) = user.sqlite_connection(uid) {
            let shared_views: Vec<WorkspaceSharedViewTable> = resp
              .shared_views
              .iter()
              .map(|shared_view| WorkspaceSharedViewTable {
                uid,
                workspace_id: workspace_id.to_string(),
                view_id: shared_view.view_id.to_string(),
                permission_id: shared_view.access_level as i32,
                created_at: None,
              })
              .collect();
            let _ = replace_all_workspace_shared_views(
              &mut conn,
              &cloud_workspace_id.to_string(),
              uid,
              &shared_views,
            );

            let repeated_shared_view_response = RepeatedSharedViewResponsePB {
              shared_views: resp
                .shared_views
                .into_iter()
                .filter_map(|shared_view| {
                  let view = all_views
                    .iter()
                    .find(|view| view.id == shared_view.view_id.to_string())?;
                  Some(SharedViewPB {
                    view: view_pb_with_all_child_views(view.clone(), &|parent_id| {
                      all_views
                        .iter()
                        .filter(|v| v.parent_view_id == *parent_id)
                        .cloned()
                        .collect()
                    }),
                    access_level: AFAccessLevelPB::from(shared_view.access_level),
                  })
                })
                .collect(),
            };

            // Notify UI to refresh the shared views
            folder_notification_builder(workspace_id, FolderNotification::DidUpdateSharedViews)
              .payload(repeated_shared_view_response)
              .send();
          }
        }
      }
    });

    let local_result = RepeatedSharedViewResponsePB {
      shared_views: local_shared_views.clone(),
    };

    Ok(local_result)
  }

  /// Get all the shared views of the workspace.
  ///
  /// This function will return all the shared views of the workspace, including the child views of the shared views.
  pub async fn get_flatten_shared_pages(&self) -> FlowyResult<Vec<ViewPB>> {
    let shared_pages = self.get_shared_pages().await?;
    let mut flattened_views = Vec::new();

    for shared_view in shared_pages.shared_views {
      // Add the parent view
      let parent_view = shared_view.view;
      let child_views = parent_view.child_views.clone();
      flattened_views.push(ViewPB {
        child_views: vec![], // Remove child views to flatten the structure
        ..parent_view
      });

      // Recursively add all child views
      Self::flatten_child_views(&child_views, &mut flattened_views);
    }

    Ok(flattened_views)
  }

  pub async fn get_shared_view_section(&self, view_id: &str) -> FlowyResult<SharedViewSectionPB> {
    const MAX_LOOP_COUNT: usize = 20;
    let mut loop_count = 0;
    let mut current_view_id = view_id.to_string();

    let lock = self
      .mutex_folder
      .load_full()
      .ok_or_else(folder_not_init_error)?;
    let folder = lock.read().await;

    let flattened_shared_views = self.get_flatten_shared_pages().await?;

    // if the view is in the flattened_shared_views, return the section
    if flattened_shared_views.iter().any(|view| view.id == view_id) {
      return Ok(SharedViewSectionPB::SharedSection);
    }

    loop {
      if loop_count >= MAX_LOOP_COUNT {
        return Ok(SharedViewSectionPB::PublicSection);
      }
      loop_count += 1;

      let view = folder
        .get_view(&current_view_id)
        .ok_or_else(|| FlowyError::record_not_found().with_context("View not found"))?;

      if let Some(space_info) = view.space_info() {
        return match space_info.space_permission {
          SpacePermission::PublicToAll => Ok(SharedViewSectionPB::PublicSection),
          _ => Ok(SharedViewSectionPB::PrivateSection),
        };
      }

      let parent_view_id = view.parent_view_id.clone();

      // If parent_view_id is the same as current view id, return public to avoid infinite loop
      if parent_view_id == current_view_id {
        return Ok(SharedViewSectionPB::PublicSection);
      }

      current_view_id = parent_view_id;
    }
  }

  fn flatten_child_views(views: &[ViewPB], flattened_views: &mut Vec<ViewPB>) {
    for view in views {
      let child_views = view.child_views.clone();
      flattened_views.push(ViewPB {
        child_views: vec![],
        ..view.clone()
      });

      if !child_views.is_empty() {
        Self::flatten_child_views(&child_views, flattened_views);
      }
    }
  }
}

/// Return the views that belong to the workspace. The views are filtered by the trash and all the private views.
pub(crate) fn get_workspace_public_view_pbs(workspace_id: &Uuid, folder: &Folder) -> Vec<ViewPB> {
  // get the trash ids
  let trash_ids = folder
    .get_all_trash_sections()
    .into_iter()
    .map(|trash| trash.id)
    .collect::<Vec<String>>();

  // get the private view ids
  let private_view_ids = folder
    .get_all_private_sections()
    .into_iter()
    .map(|view| view.id)
    .collect::<Vec<String>>();

  let mut views = folder.get_views_belong_to(&workspace_id.to_string());
  // filter the views that are in the trash and all the private views
  views.retain(|view| !trash_ids.contains(&view.id) && !private_view_ids.contains(&view.id));

  views
    .into_iter()
    .map(|view| {
      // Get child views
      let mut child_views: Vec<Arc<View>> =
        folder.get_views_belong_to(&view.id).into_iter().collect();
      child_views.retain(|view| !trash_ids.contains(&view.id));
      view_pb_with_child_views(view, child_views)
    })
    .collect()
}

/// Get all the child views belong to the view id, including the child views of the child views.
fn get_all_child_view_ids(folder: &Folder, view_id: &str) -> Vec<String> {
  folder
    .get_view_recursively(view_id)
    .iter()
    .map(|view| view.id.clone())
    .collect()
}

/// Get the current private views of the user.
pub(crate) fn get_workspace_private_view_pbs(workspace_id: &Uuid, folder: &Folder) -> Vec<ViewPB> {
  // get the trash ids
  let trash_ids = folder
    .get_all_trash_sections()
    .into_iter()
    .map(|trash| trash.id)
    .collect::<Vec<String>>();

  // get the private view ids
  let private_view_ids = folder
    .get_my_private_sections()
    .into_iter()
    .map(|view| view.id)
    .collect::<Vec<String>>();

  let mut views = folder.get_views_belong_to(&workspace_id.to_string());
  // filter the views that are in the trash and not in the private view ids
  views.retain(|view| !trash_ids.contains(&view.id) && private_view_ids.contains(&view.id));

  views
    .into_iter()
    .map(|view| {
      // Get child views
      let mut child_views: Vec<Arc<View>> =
        folder.get_views_belong_to(&view.id).into_iter().collect();
      child_views.retain(|view| !trash_ids.contains(&view.id));
      view_pb_with_child_views(view, child_views)
    })
    .collect()
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum FolderInitDataSource {
  /// It means using the data stored on local disk to initialize the folder
  LocalDisk { create_if_not_exist: bool },
  /// If there is no data stored on local disk, we will use the data from the server to initialize the folder
  Cloud(Vec<u8>),
}

impl Display for FolderInitDataSource {
  fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
    match self {
      FolderInitDataSource::LocalDisk { .. } => f.write_fmt(format_args!("LocalDisk")),
      FolderInitDataSource::Cloud(_) => f.write_fmt(format_args!("Cloud")),
    }
  }
}

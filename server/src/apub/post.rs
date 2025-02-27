use crate::{
  apub::{
    activities::{populate_object_props, send_activity_to_community},
    create_apub_response, create_apub_tombstone_response, create_tombstone,
    extensions::page_extension::PageExtension,
    fetcher::{get_or_fetch_and_upsert_remote_community, get_or_fetch_and_upsert_remote_user},
    ActorType, ApubLikeableType, ApubObjectType, FromApub, PageExt, ToApub,
  },
  blocking,
  routes::DbPoolParam,
  DbPool, LemmyError,
};
use activitystreams::{
  activity::{Create, Delete, Dislike, Like, Remove, Undo, Update},
  BaseBox,
};
use activitystreams_ext::Ext1;
use activitystreams_new::{
  context,
  object::{kind::PageType, Image, Page, Tombstone},
  prelude::*,
  primitives::{XsdAnyUri, XsdDateTime},
};
use actix_web::{body::Body, client::Client, web, HttpResponse};
use lemmy_db::{
  community::Community,
  post::{Post, PostForm},
  user::User_,
  Crud,
};
use lemmy_utils::{convert_datetime, get_apub_protocol_string, settings::Settings};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct PostQuery {
  post_id: String,
}

/// Return the post json over HTTP.
pub async fn get_apub_post(
  info: web::Path<PostQuery>,
  db: DbPoolParam,
) -> Result<HttpResponse<Body>, LemmyError> {
  let id = info.post_id.parse::<i32>()?;
  let post = blocking(&db, move |conn| Post::read(conn, id)).await??;

  if !post.deleted {
    Ok(create_apub_response(&post.to_apub(&db).await?))
  } else {
    Ok(create_apub_tombstone_response(&post.to_tombstone()?))
  }
}

#[async_trait::async_trait(?Send)]
impl ToApub for Post {
  type Response = PageExt;

  // Turn a Lemmy post into an ActivityPub page that can be sent out over the network.
  async fn to_apub(&self, pool: &DbPool) -> Result<PageExt, LemmyError> {
    let mut page = Page::new();

    let creator_id = self.creator_id;
    let creator = blocking(pool, move |conn| User_::read(conn, creator_id)).await??;

    let community_id = self.community_id;
    let community = blocking(pool, move |conn| Community::read(conn, community_id)).await??;

    page
      // Not needed when the Post is embedded in a collection (like for community outbox)
      // TODO: need to set proper context defining sensitive/commentsEnabled fields
      // https://git.asonix.dog/Aardwolf/activitystreams/issues/5
      .set_context(context())
      .set_id(self.ap_id.parse::<XsdAnyUri>()?)
      // Use summary field to be consistent with mastodon content warning.
      // https://mastodon.xyz/@Louisa/103987265222901387.json
      .set_summary(self.name.to_owned())
      .set_published(convert_datetime(self.published).into())
      .set_to(community.actor_id)
      .set_attributed_to(creator.actor_id);

    if let Some(body) = &self.body {
      page.set_content(body.to_owned());
    }

    // TODO: hacky code because we get self.url == Some("")
    // https://github.com/LemmyNet/lemmy/issues/602
    let url = self.url.as_ref().filter(|u| !u.is_empty());
    if let Some(u) = url {
      page.set_url(u.to_owned());

      // Embeds
      let mut page_preview = Page::new();
      page_preview.set_url(u.to_owned());

      if let Some(embed_title) = &self.embed_title {
        page_preview.set_name(embed_title.to_owned());
      }

      if let Some(embed_description) = &self.embed_description {
        page_preview.set_summary(embed_description.to_owned());
      }

      if let Some(embed_html) = &self.embed_html {
        page_preview.set_content(embed_html.to_owned());
      }

      page.set_preview(page_preview.into_any_base()?);
    }

    if let Some(thumbnail_url) = &self.thumbnail_url {
      let full_url = format!(
        "{}://{}/pictshare/{}",
        get_apub_protocol_string(),
        Settings::get().hostname,
        thumbnail_url
      );

      let mut image = Image::new();
      image.set_url(full_url);
      page.set_image(image.into_any_base()?);
    }

    if let Some(u) = self.updated {
      page.set_updated(XsdDateTime::from(convert_datetime(u)));
    }

    let ext = PageExtension {
      comments_enabled: !self.locked,
      sensitive: self.nsfw,
    };
    Ok(Ext1::new(page, ext))
  }

  fn to_tombstone(&self) -> Result<Tombstone, LemmyError> {
    create_tombstone(
      self.deleted,
      &self.ap_id,
      self.updated,
      PageType.to_string(),
    )
  }
}

#[async_trait::async_trait(?Send)]
impl FromApub for PostForm {
  type ApubType = PageExt;

  /// Parse an ActivityPub page received from another instance into a Lemmy post.
  async fn from_apub(
    page: &PageExt,
    client: &Client,
    pool: &DbPool,
  ) -> Result<PostForm, LemmyError> {
    let ext = &page.ext_one;
    let creator_actor_id = page
      .inner
      .attributed_to
      .as_ref()
      .unwrap()
      .as_single_xsd_any_uri()
      .unwrap();

    let creator = get_or_fetch_and_upsert_remote_user(creator_actor_id, client, pool).await?;

    let community_actor_id = page
      .inner
      .to
      .as_ref()
      .unwrap()
      .as_single_xsd_any_uri()
      .unwrap()
      .as_str();

    let community =
      get_or_fetch_and_upsert_remote_community(community_actor_id, client, pool).await?;

    let thumbnail_url = match &page.inner.image {
      Some(any_image) => Image::from_any_base(any_image.to_owned().as_one().unwrap().to_owned())?
        .unwrap()
        .url
        .unwrap()
        .as_single_xsd_any_uri()
        .map(|u| u.to_string()),
      None => None,
    };

    let (embed_title, embed_description, embed_html) = match page.inner.preview() {
      Some(preview) => {
        let preview_page = Page::from_any_base(preview.as_one().unwrap().to_owned())?.unwrap();
        let name = preview_page
          .name()
          .map(|n| n.as_single_xsd_string().unwrap().to_string());
        let summary = preview_page
          .summary()
          .map(|s| s.as_single_xsd_string().unwrap().to_string());
        let content = preview_page
          .content()
          .map(|c| c.as_single_xsd_string().unwrap().to_string());
        (name, summary, content)
      }
      None => (None, None, None),
    };

    let url = page
      .inner
      .url
      .as_ref()
      .map(|u| u.as_single_xsd_string().unwrap().to_string());
    let body = page
      .inner
      .content
      .as_ref()
      .map(|c| c.as_single_xsd_string().unwrap().to_string());
    Ok(PostForm {
      name: page
        .inner
        .summary
        .as_ref()
        .unwrap()
        .as_single_xsd_string()
        .unwrap()
        .to_string(),
      url,
      body,
      creator_id: creator.id,
      community_id: community.id,
      removed: None,
      locked: Some(!ext.comments_enabled),
      published: page
        .inner
        .published
        .as_ref()
        .map(|u| u.as_ref().to_owned().naive_local()),
      updated: page
        .inner
        .updated
        .as_ref()
        .map(|u| u.as_ref().to_owned().naive_local()),
      deleted: None,
      nsfw: ext.sensitive,
      stickied: None, // -> put it in "featured" collection of the community
      embed_title,
      embed_description,
      embed_html,
      thumbnail_url,
      ap_id: page.inner.id().unwrap().to_string(),
      local: false,
    })
  }
}

#[async_trait::async_trait(?Send)]
impl ApubObjectType for Post {
  /// Send out information about a newly created post, to the followers of the community.
  async fn send_create(
    &self,
    creator: &User_,
    client: &Client,
    pool: &DbPool,
  ) -> Result<(), LemmyError> {
    let page = self.to_apub(pool).await?;

    let community_id = self.community_id;
    let community = blocking(pool, move |conn| Community::read(conn, community_id)).await??;

    let id = format!("{}/create/{}", self.ap_id, uuid::Uuid::new_v4());

    let mut create = Create::new();
    populate_object_props(
      &mut create.object_props,
      vec![community.get_followers_url()],
      &id,
    )?;
    create
      .create_props
      .set_actor_xsd_any_uri(creator.actor_id.to_owned())?
      .set_object_base_box(BaseBox::from_concrete(page)?)?;

    send_activity_to_community(
      creator,
      &community,
      vec![community.get_shared_inbox_url()],
      create,
      client,
      pool,
    )
    .await?;
    Ok(())
  }

  /// Send out information about an edited post, to the followers of the community.
  async fn send_update(
    &self,
    creator: &User_,
    client: &Client,
    pool: &DbPool,
  ) -> Result<(), LemmyError> {
    let page = self.to_apub(pool).await?;

    let community_id = self.community_id;
    let community = blocking(pool, move |conn| Community::read(conn, community_id)).await??;

    let id = format!("{}/update/{}", self.ap_id, uuid::Uuid::new_v4());

    let mut update = Update::new();
    populate_object_props(
      &mut update.object_props,
      vec![community.get_followers_url()],
      &id,
    )?;
    update
      .update_props
      .set_actor_xsd_any_uri(creator.actor_id.to_owned())?
      .set_object_base_box(BaseBox::from_concrete(page)?)?;

    send_activity_to_community(
      creator,
      &community,
      vec![community.get_shared_inbox_url()],
      update,
      client,
      pool,
    )
    .await?;
    Ok(())
  }

  async fn send_delete(
    &self,
    creator: &User_,
    client: &Client,
    pool: &DbPool,
  ) -> Result<(), LemmyError> {
    let page = self.to_apub(pool).await?;

    let community_id = self.community_id;
    let community = blocking(pool, move |conn| Community::read(conn, community_id)).await??;

    let id = format!("{}/delete/{}", self.ap_id, uuid::Uuid::new_v4());
    let mut delete = Delete::default();

    populate_object_props(
      &mut delete.object_props,
      vec![community.get_followers_url()],
      &id,
    )?;

    delete
      .delete_props
      .set_actor_xsd_any_uri(creator.actor_id.to_owned())?
      .set_object_base_box(BaseBox::from_concrete(page)?)?;

    send_activity_to_community(
      creator,
      &community,
      vec![community.get_shared_inbox_url()],
      delete,
      client,
      pool,
    )
    .await?;
    Ok(())
  }

  async fn send_undo_delete(
    &self,
    creator: &User_,
    client: &Client,
    pool: &DbPool,
  ) -> Result<(), LemmyError> {
    let page = self.to_apub(pool).await?;

    let community_id = self.community_id;
    let community = blocking(pool, move |conn| Community::read(conn, community_id)).await??;

    let id = format!("{}/delete/{}", self.ap_id, uuid::Uuid::new_v4());
    let mut delete = Delete::default();

    populate_object_props(
      &mut delete.object_props,
      vec![community.get_followers_url()],
      &id,
    )?;

    delete
      .delete_props
      .set_actor_xsd_any_uri(creator.actor_id.to_owned())?
      .set_object_base_box(BaseBox::from_concrete(page)?)?;

    // TODO
    // Undo that fake activity
    let undo_id = format!("{}/undo/delete/{}", self.ap_id, uuid::Uuid::new_v4());
    let mut undo = Undo::default();

    populate_object_props(
      &mut undo.object_props,
      vec![community.get_followers_url()],
      &undo_id,
    )?;

    undo
      .undo_props
      .set_actor_xsd_any_uri(creator.actor_id.to_owned())?
      .set_object_base_box(delete)?;

    send_activity_to_community(
      creator,
      &community,
      vec![community.get_shared_inbox_url()],
      undo,
      client,
      pool,
    )
    .await?;
    Ok(())
  }

  async fn send_remove(
    &self,
    mod_: &User_,
    client: &Client,
    pool: &DbPool,
  ) -> Result<(), LemmyError> {
    let page = self.to_apub(pool).await?;

    let community_id = self.community_id;
    let community = blocking(pool, move |conn| Community::read(conn, community_id)).await??;

    let id = format!("{}/remove/{}", self.ap_id, uuid::Uuid::new_v4());
    let mut remove = Remove::default();

    populate_object_props(
      &mut remove.object_props,
      vec![community.get_followers_url()],
      &id,
    )?;

    remove
      .remove_props
      .set_actor_xsd_any_uri(mod_.actor_id.to_owned())?
      .set_object_base_box(BaseBox::from_concrete(page)?)?;

    send_activity_to_community(
      mod_,
      &community,
      vec![community.get_shared_inbox_url()],
      remove,
      client,
      pool,
    )
    .await?;
    Ok(())
  }

  async fn send_undo_remove(
    &self,
    mod_: &User_,
    client: &Client,
    pool: &DbPool,
  ) -> Result<(), LemmyError> {
    let page = self.to_apub(pool).await?;

    let community_id = self.community_id;
    let community = blocking(pool, move |conn| Community::read(conn, community_id)).await??;

    let id = format!("{}/remove/{}", self.ap_id, uuid::Uuid::new_v4());
    let mut remove = Remove::default();

    populate_object_props(
      &mut remove.object_props,
      vec![community.get_followers_url()],
      &id,
    )?;

    remove
      .remove_props
      .set_actor_xsd_any_uri(mod_.actor_id.to_owned())?
      .set_object_base_box(BaseBox::from_concrete(page)?)?;

    // Undo that fake activity
    let undo_id = format!("{}/undo/remove/{}", self.ap_id, uuid::Uuid::new_v4());
    let mut undo = Undo::default();

    populate_object_props(
      &mut undo.object_props,
      vec![community.get_followers_url()],
      &undo_id,
    )?;

    undo
      .undo_props
      .set_actor_xsd_any_uri(mod_.actor_id.to_owned())?
      .set_object_base_box(remove)?;

    send_activity_to_community(
      mod_,
      &community,
      vec![community.get_shared_inbox_url()],
      undo,
      client,
      pool,
    )
    .await?;
    Ok(())
  }
}

#[async_trait::async_trait(?Send)]
impl ApubLikeableType for Post {
  async fn send_like(
    &self,
    creator: &User_,
    client: &Client,
    pool: &DbPool,
  ) -> Result<(), LemmyError> {
    let page = self.to_apub(pool).await?;

    let community_id = self.community_id;
    let community = blocking(pool, move |conn| Community::read(conn, community_id)).await??;

    let id = format!("{}/like/{}", self.ap_id, uuid::Uuid::new_v4());

    let mut like = Like::new();
    populate_object_props(
      &mut like.object_props,
      vec![community.get_followers_url()],
      &id,
    )?;
    like
      .like_props
      .set_actor_xsd_any_uri(creator.actor_id.to_owned())?
      .set_object_base_box(BaseBox::from_concrete(page)?)?;

    send_activity_to_community(
      &creator,
      &community,
      vec![community.get_shared_inbox_url()],
      like,
      client,
      pool,
    )
    .await?;
    Ok(())
  }

  async fn send_dislike(
    &self,
    creator: &User_,
    client: &Client,
    pool: &DbPool,
  ) -> Result<(), LemmyError> {
    let page = self.to_apub(pool).await?;

    let community_id = self.community_id;
    let community = blocking(pool, move |conn| Community::read(conn, community_id)).await??;

    let id = format!("{}/dislike/{}", self.ap_id, uuid::Uuid::new_v4());

    let mut dislike = Dislike::new();
    populate_object_props(
      &mut dislike.object_props,
      vec![community.get_followers_url()],
      &id,
    )?;
    dislike
      .dislike_props
      .set_actor_xsd_any_uri(creator.actor_id.to_owned())?
      .set_object_base_box(BaseBox::from_concrete(page)?)?;

    send_activity_to_community(
      &creator,
      &community,
      vec![community.get_shared_inbox_url()],
      dislike,
      client,
      pool,
    )
    .await?;
    Ok(())
  }

  async fn send_undo_like(
    &self,
    creator: &User_,
    client: &Client,
    pool: &DbPool,
  ) -> Result<(), LemmyError> {
    let page = self.to_apub(pool).await?;

    let community_id = self.community_id;
    let community = blocking(pool, move |conn| Community::read(conn, community_id)).await??;

    let id = format!("{}/like/{}", self.ap_id, uuid::Uuid::new_v4());

    let mut like = Like::new();
    populate_object_props(
      &mut like.object_props,
      vec![community.get_followers_url()],
      &id,
    )?;
    like
      .like_props
      .set_actor_xsd_any_uri(creator.actor_id.to_owned())?
      .set_object_base_box(BaseBox::from_concrete(page)?)?;

    // TODO
    // Undo that fake activity
    let undo_id = format!("{}/undo/like/{}", self.ap_id, uuid::Uuid::new_v4());
    let mut undo = Undo::default();

    populate_object_props(
      &mut undo.object_props,
      vec![community.get_followers_url()],
      &undo_id,
    )?;

    undo
      .undo_props
      .set_actor_xsd_any_uri(creator.actor_id.to_owned())?
      .set_object_base_box(like)?;

    send_activity_to_community(
      &creator,
      &community,
      vec![community.get_shared_inbox_url()],
      undo,
      client,
      pool,
    )
    .await?;
    Ok(())
  }
}

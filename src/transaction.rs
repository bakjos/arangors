use maybe_async::maybe_async;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use typed_builder::TypedBuilder;
use url::Url;

use crate::{
    aql::Cursor,
    client::ClientExt,
    collection::response::Info,
    response::{deserialize_response, ArangoResult},
    AqlQuery, ClientError, Collection,
};

pub const TRANSACTION_HEADER: &str = "x-arango-trx-id";

#[derive(Debug, Serialize, Deserialize, TypedBuilder)]
#[builder(doc)]
pub struct TransactionCollections {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[builder(default, setter(strip_option))]
    read: Option<Vec<String>>,

    write: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, TypedBuilder)]
#[serde(rename_all = "camelCase")]
#[builder(doc)]
pub struct TransactionSettings {
    collections: TransactionCollections,

    #[builder(default, setter(strip_option))]
    #[serde(skip_serializing_if = "Option::is_none")]
    wait_for_sync: Option<bool>,

    #[builder(default = true)]
    allow_implicit: bool,

    #[builder(default, setter(strip_option))]
    #[serde(skip_serializing_if = "Option::is_none")]
    lock_timeout: Option<usize>,

    #[builder(default, setter(strip_option))]
    #[serde(skip_serializing_if = "Option::is_none")]
    max_transaction_size: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Running,
    Committed,
    Aborted,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ArangoTransaction {
    pub id: String,
    pub status: Status,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TransactionState {
    pub id: String,
    pub state: Status,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TransactionList {
    pub transactions: Vec<TransactionState>,
}

pub struct Transaction<C: ClientExt> {
    id: String,
    status: Status,
    session: Arc<C>,
    base_url: Url,
}

impl<C> Transaction<C>
where
    C: ClientExt,
{
    pub(crate) fn new(tx: ArangoTransaction, session: Arc<C>, base_url: Url) -> Self {
        Transaction {
            id: tx.id,
            status: tx.status,
            session,
            base_url,
        }
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    pub fn id(&self) -> &String {
        &self.id
    }

    pub fn url(&self) -> &Url {
        &self.base_url
    }

    pub fn session(&self) -> Arc<C> {
        Arc::clone(&self.session)
    }

    #[maybe_async]
    pub async fn commit_transaction(self) -> Result<Status, ClientError> {
        let url = self
            .base_url
            .join(&format!("_api/transaction/{}", self.id))
            .unwrap();

        let resp = self.session.put(url, "").await?;

        let result: ArangoResult<ArangoTransaction> = deserialize_response(resp.body())?;

        Ok(result.unwrap().status)
    }

    #[maybe_async]
    pub async fn commit(&self) -> Result<Status, ClientError> {
        let url = self
            .base_url
            .join(&format!("_api/transaction/{}", self.id))
            .unwrap();

        let resp = self.session.put(url, "").await?;

        let result: ArangoResult<ArangoTransaction> = deserialize_response(resp.body())?;

        Ok(result.unwrap().status)
    }

    #[maybe_async]
    pub async fn abort(&self) -> Result<Status, ClientError> {
        let url = self
            .base_url
            .join(&format!("_api/transaction/{}", self.id))
            .unwrap();

        let resp = self.session.delete(url, "").await?;

        let result: ArangoResult<ArangoTransaction> = deserialize_response(resp.body())?;

        Ok(result.unwrap().status)
    }

    #[maybe_async]
    pub async fn collection(&self, name: &str) -> Result<Collection<C>, ClientError> {
        let url = self
            .base_url
            .join(&format!("_api/collection/{}", name))
            .unwrap();
        let resp: Info = deserialize_response(self.session.get(url, "").await?.body())?;
        Ok(Collection::from_transaction_response(self, &resp))
    }

    #[maybe_async]
    pub async fn aql_query_batch<R>(&self, aql: AqlQuery<'_>) -> Result<Cursor<R>, ClientError>
    where
        R: DeserializeOwned,
    {
        let url = self.base_url.join("_api/cursor").unwrap();
        let resp = self
            .session
            .post(url, &serde_json::to_string(&aql)?)
            .await?;
        deserialize_response(resp.body())
    }

    #[maybe_async]
    pub async fn aql_next_batch<R>(&self, cursor_id: &str) -> Result<Cursor<R>, ClientError>
    where
        R: DeserializeOwned,
    {
        let url = self
            .base_url
            .join(&format!("_api/cursor/{}", cursor_id))
            .unwrap();
        let resp = self.session.put(url, "").await?;

        deserialize_response(resp.body())
    }

    #[maybe_async]
    async fn aql_fetch_all<R>(&self, response: Cursor<R>) -> Result<Vec<R>, ClientError>
    where
        R: DeserializeOwned,
    {
        let mut response_cursor = response;
        let mut results: Vec<R> = Vec::new();
        loop {
            if response_cursor.more {
                let id = response_cursor.id.unwrap().clone();
                results.extend(response_cursor.result.into_iter());
                response_cursor = self.aql_next_batch(id.as_str()).await?;
            } else {
                break;
            }
        }
        Ok(results)
    }

    /// Execute AQL query fetch all results.
    ///
    /// DO NOT do this when the count of results is too large that network or
    /// memory resources cannot afford.
    ///
    /// DO NOT set a small batch size, otherwise clients will have to make many
    /// HTTP requests.
    ///
    /// # Note
    /// this function would make a request to arango server.
    #[maybe_async]
    pub async fn aql_query<R>(&self, aql: AqlQuery<'_>) -> Result<Vec<R>, ClientError>
    where
        R: DeserializeOwned,
    {
        let response = self.aql_query_batch(aql).await?;
        if response.more {
            self.aql_fetch_all(response).await
        } else {
            Ok(response.result)
        }
    }

    /// Similar to `aql_query`, except that this method only accept a string of
    /// AQL query.
    ///
    /// # Note
    /// this function would make a request to arango server.
    #[maybe_async]
    pub async fn aql_str<R>(&self, query: &str) -> Result<Vec<R>, ClientError>
    where
        R: DeserializeOwned,
    {
        let aql = AqlQuery::builder().query(query).build();
        self.aql_query(aql).await
    }

    /// Similar to `aql_query`, except that this method only accept a string of
    /// AQL query, with additional bind vars.
    ///
    /// # Note
    /// this function would make a request to arango server.
    #[maybe_async]
    pub async fn aql_bind_vars<R>(
        &self,
        query: &str,
        bind_vars: HashMap<&str, Value>,
    ) -> Result<Vec<R>, ClientError>
    where
        R: DeserializeOwned,
    {
        let aql = AqlQuery::builder()
            .query(query)
            .bind_vars(bind_vars)
            .build();
        self.aql_query(aql).await
    }
}
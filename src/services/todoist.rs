use actix_web::web::Bytes;
use actix_web::{web, HttpRequest, HttpResponse, Responder};
use cloud_pubsub::{EncodedMessage, Topic};
use data_encoding::BASE64;

use ring::hmac;
use serde::{Deserialize, Serialize};
use serde_json;

use reqwest::header::AUTHORIZATION;
use std::{collections::HashMap, sync::Arc};

use anyhow::{anyhow, Result};
use log::debug;
use serde_json::Value;

#[derive(Deserialize, Clone)]
pub struct TodoistConfig {
    pub client_id: String,
    pub client_secret: String,
    pub access_token: String,
    #[serde(default = "default_topic")]
    pub topic: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TodoistEvent {
    user_id: String,
    version: String,
    initiator: Value,
    event_name: String,
    event_data: Value,
}


#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SectionOrItemEvent {
    id: String,
    parent_id: Option<String>,
    project_id: Option<String>,
    section_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProjectEvent {
    id: String,
    name: String,
    parent_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TodoistProject {
    id: String,
    name: String,
    parent_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TodoistSection {
    id: String,
    name: String,
}

pub struct ExtractedAttributes {
    pub id: String,
    pub project_name: String,
    pub parent_name: String,
    pub parent_parent_name: String,
    pub section_name: String,
}

pub async fn webhook(
    req: HttpRequest,
    body: Bytes,
    topic: web::Data<Arc<Topic>>,
    config: web::Data<TodoistConfig>,
) -> impl Responder {
    authorize_request(&body, req, config.client_secret.clone())
        .unwrap();

    // For simplicity, TodoistEvent only contains only some data
    //  payload is used for the complete publishing.
    let event: TodoistEvent = serde_json::from_slice(&body).unwrap();
    let event_name = event.event_name.clone();
    let payload: serde_json::Value =
        serde_json::from_slice(&body).unwrap();
    let projects = match get_projects(&config).await {
        Ok(p) => p,
        Err(e) => {
            log::error!("Failed to fetch projects: {}", e);
            return HttpResponse::InternalServerError().finish();
        }
    };

    debug!("event: {:?}", event);
    debug!("projects: {:?}", projects);

    let attr = if event.event_name.starts_with("project:") {
        extract_project_attributes(&event, projects).await
    } else {
        extract_item_section_attributes(&event, &config, projects)
            .await
    };

    log::info!(
        "message published: event_name={}, project_name={}, parent_name={}, parent_parent_name={}, section_name={}",
        &event_name,
        &attr.project_name,
        &attr.parent_name,
        &attr.parent_parent_name,
        &attr.section_name
    );

    topic
        .clone()
        .publish_message(EncodedMessage::new(
            &payload.clone(),
            Some(HashMap::from([
                ("event_name".to_string(), event_name.clone()),
                ("project_name".to_string(), attr.project_name),
                ("parent_name".to_string(), attr.parent_name),
                (
                    "parent_parent_name".to_string(),
                    attr.parent_parent_name,
                ),
                ("section_name".to_string(), attr.section_name),
            ])),
            Some(attr.id),
        ))
        .await
        .unwrap();

    HttpResponse::Ok().finish()
}

async fn extract_project_attributes(
    event: &TodoistEvent,
    projects: Vec<TodoistProject>,
) -> ExtractedAttributes {
    // take extract inner value
    let project: ProjectEvent = match serde_json::from_value(event.event_data.clone()) {
        Ok(p) => p,
        Err(e) => panic!("Failed to extract project event: {}", e),
    };

    let parent = match &projects
        .iter()
        .find(|p| Some(p.id.clone()) == project.parent_id)
    {
        None => None,
        Some(p) => Some(p.clone()),
    };

    let parent_name = match &parent {
        None => "".to_string(),
        Some(p) => p.name.clone(),
    };

    let parent_parent_name = match &parent {
        None => "".to_string(),
        Some(p) => match &projects
            .iter()
            .find(|p_2| Some(p_2.id.clone()) == p.parent_id)
        {
            None => "".to_string(),
            Some(p) => p.name.clone(),
        },
    };

    ExtractedAttributes {
        id: project.id.clone(),
        project_name: project.name.clone(),
        parent_name,
        parent_parent_name,
        section_name: "".to_string(),
    }
}

async fn extract_item_section_attributes(
    event: &TodoistEvent,
    config: &TodoistConfig,
    projects: Vec<TodoistProject>,
) -> ExtractedAttributes {
    let event_data: SectionOrItemEvent = match serde_json::from_value(event.event_data.clone()) {
        Ok(e) => e,
        Err(e) => panic!("Failed to extract section or item event: {}", e),
    };

    let cur_project = match event_data.clone().project_id {
        None => None,
        Some(project_id) => match &projects
            .iter()
            .find(|project| project.id == project_id)
        {
            None => None,
            Some(project) => Some(<&TodoistProject>::clone(project)),
        },
    };

    debug!("project: {:?}", cur_project);

    let project_name = match &cur_project {
        None => "".to_string(),
        Some(project) => project.name.clone(),
    };

    let parent = match &cur_project {
        None => None,
        Some(project) => match &projects.iter().find(|p| {
            Some(p.id.clone()) == project.parent_id
        }) {
            None => None,
            Some(project) => Some(project.clone()),
        },
    };
    let parent_name = match &parent {
        None => "".to_string(),
        Some(project) => project.name.clone(),
    };

    let parent_parent_name = match &parent {
        None => "".to_string(),
        Some(project) => match &projects.iter().find(|p| {
            Some(p.id.clone()) == project.parent_id
        }) {
            None => "".to_string(),
            Some(project) => project.name.clone(),
        },
    };

    let section_name = if event.event_name.starts_with("section:") {
        match get_section(event_data.id.clone(), &config).await {
            Ok(section) => section.name.clone(),
            _ => "".to_string(),
        }
    } else {
        match &event_data.section_id {
            None => "".to_string(),
            Some(section_id) => {
                match get_section(section_id.clone(), &config).await {
                    Ok(section) => section.name.clone(),
                    _ => "".to_string(),
                }
            }
        }
    };

    ExtractedAttributes {
        id: event_data.id.clone(),
        project_name,
        parent_name,
        parent_parent_name,
        section_name,
    }
}

async fn get_projects(
    config: &TodoistConfig,
) -> Result<Vec<TodoistProject>> {
    let response = reqwest::Client::new()
        .get("https://api.todoist.com/api/v1/project")
        .header(
            AUTHORIZATION,
            format!("Bearer {}", config.access_token),
        )
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!(
            "Todoist API returned status {}: {}",
            status,
            body
        ));
    }

    Ok(response.json().await?)
}

async fn get_section(
    id: String,
    config: &TodoistConfig,
) -> Result<TodoistSection> {
    let response = reqwest::Client::new()
        .get(format!(
            "https://api.todoist.com/rest/v1/sections/{}",
            id
        ))
        .header(
            AUTHORIZATION,
            format!("Bearer {}", config.access_token),
        )
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!(
            "Todoist API returned status {}: {}",
            status,
            body
        ));
    }

    Ok(response.json().await?)
}

fn authorize_request(
    body: &[u8],
    request: HttpRequest,
    client_secret: String,
) -> Result<()> {
    let signature = request
        .headers()
        .get("X-Todoist-HMAC-SHA256")
        .ok_or(anyhow!("Missing header."))?
        .to_str()?;

    let key_value = client_secret.as_bytes();
    let key = hmac::Key::new(hmac::HMAC_SHA256, &key_value);

    let hash = BASE64.encode(hmac::sign(&key, &body).as_ref());

    if hash == String::from(signature) {
        Ok(())
    } else {
        Err(anyhow!("Invalid Signature."))
    }
}

fn default_topic() -> String {
    "todoist".to_string()
}

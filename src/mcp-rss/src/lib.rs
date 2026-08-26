//! mcp-rss - MCP server that provides RSS tooling.

#![deny(unsafe_code)]
#![deny(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::unreachable
)]
#![deny(clippy::arithmetic_side_effects)]
#![deny(clippy::todo)]
#![deny(clippy::allow_attributes_without_reason)]

use readabilityrs::{Readability, ReadabilityOptions};
use rmcp::{
  ErrorData, Json, handler::server::wrapper::Parameters, schemars, tool,
  tool_router,
};
use scraper::Html;
use std::time::Duration;

#[allow(clippy::unwrap_used, reason = "these are all valid selectors")]
mod selectors {
  use scraper::Selector;

  lazy_static::lazy_static! {
    pub static ref BODY_SELECTOR: Selector = Selector::parse("body").unwrap();

    pub static ref CONTENT_SELECTORS: Vec<Selector> = [
        "article",
        "main",
        ".post-content",
        ".entry-content",
        ".article-body",
        "#article-content",
        ".content",
        "body",
      ]
        .map(|selector| Selector::parse(selector).unwrap())
        .to_vec();
  }
}

/// MCP server that provides RSS tooling.
#[derive(Debug, Clone)]
pub struct RssServer {
  http: reqwest::Client,
}

impl RssServer {
  pub fn new() -> anyhow::Result<Self> {
    Ok(Self {
      http: reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(format!(
          "{}/{}",
          env!("CARGO_PKG_NAME"),
          env!("CARGO_PKG_VERSION")
        ))
        .build()?,
    })
  }
}

#[derive(serde::Deserialize, schemars::JsonSchema, Default)]
struct GetArticlesInput {
  /// List of RSS/Atom feed URLs to fetch articles from
  feeds: Vec<String>,
  /// If provided, only articles published after this time will be returned (ISO 8601 format)
  time_from: Option<String>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct Article {
  /// Canonical URL of the article
  link: String,
  /// Article title
  title: String,
  /// Feed-specific identifier (GUID)
  id: String,
  /// Publication date, if available
  published: Option<String>,
  /// Feed-specific summary/description
  description: Option<String>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct GetArticlesOutput {
  /// List of articles
  articles: Vec<Article>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct FetchArticleInput {
  /// URL of the article to fetch
  url: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct FetchArticleOutput {
  /// Cleaned text content of the article
  content: String,
}

#[tool_router(server_handler)]
impl RssServer {
  /// Get articles from RSS/Atom feeds, optionally filtered by publication date.
  ///
  /// Returns a list of article URLs (links) from the specified feeds.
  #[tool(
    name = "get_articles",
    description = "Get articles from RSS/Atom feeds, optionally filtered by publication date."
  )]
  async fn get_articles(
    &self,
    Parameters(input): Parameters<GetArticlesInput>,
  ) -> Result<Json<GetArticlesOutput>, ErrorData> {
    if input.feeds.is_empty() {
      return Err(ErrorData::invalid_params(
        "no feeds provided; the 'feeds' argument must contain at least one URL",
        None,
      ));
    }

    let parsed_time = if let Some(ref iso) = input.time_from {
      match chrono::DateTime::parse_from_rfc3339(iso) {
        Ok(dt) => Some(dt),
        Err(e) => {
          return Err(ErrorData::invalid_params(
            format!("invalid ISO 8601 'time_from' value {iso:?}: {e}"),
            None,
          ));
        }
      }
    } else {
      None
    };

    let mut articles: Vec<Article> = Vec::new();

    for feed_url in input.feeds {
      let feed_content = {
        let http = &self.http;
        match http.get(&feed_url).send().await {
          Ok(resp) => match resp.text().await {
            Ok(text) => text,
            Err(e) => {
              return Err(ErrorData::internal_error(
                format!("failed to read feed {feed_url}: {e}"),
                None,
              ));
            }
          },
          Err(e) => {
            return Err(ErrorData::internal_error(
              format!("failed to connect to feed {feed_url}: {e}"),
              None,
            ));
          }
        }
      };

      let feed = match rss::Channel::read_from(feed_content.as_bytes()) {
        Ok(channel) => channel,
        Err(e) => {
          return Err(ErrorData::internal_error(
            format!("feed {feed_url} could not be parsed as RSS/Atom: {e}"),
            None,
          ));
        }
      };

      for item in feed.items() {
        let link = item.link().map(|l| l.to_string());
        let id = item.guid().map(|g| g.value.clone()).unwrap_or_default();
        let title = item.title().map(|t| t.to_string());
        let published = item
          .pub_date()
          .and_then(|d| chrono::DateTime::parse_from_rfc2822(d).ok());
        let description = item.description().map(|d| d.to_string());

        let include = match (published.as_ref(), &parsed_time) {
          (Some(item_date), Some(from_date)) => item_date >= from_date,
          (Some(_), None) => true,
          (None, Some(_)) => true,
          (None, None) => true,
        };

        if include && let Some(link) = link {
          articles.push(Article {
            link,
            title: title.unwrap_or_default(),
            id,
            published: published.map(|d| d.to_rfc3339()),
            description,
          });
        }
      }
    }

    // Deduplicate by link
    let mut seen = std::collections::HashSet::new();
    articles.retain(|a| seen.insert(a.link.clone()));

    Ok(Json(GetArticlesOutput { articles }))
  }

  /// Fetch the content of a single article from a URL.
  ///
  /// Returns cleaned text content extracted from the HTML page.
  #[tool(
    name = "fetch_article",
    description = "Fetch the content of a single article from a URL."
  )]
  async fn fetch_article(
    &self,
    Parameters(input): Parameters<FetchArticleInput>,
  ) -> Result<Json<FetchArticleOutput>, ErrorData> {
    if input.url.trim().is_empty() {
      return Err(ErrorData::invalid_params("no URL provided", None));
    }

    let html = {
      let http = &self.http;
      match http.get(&input.url).send().await {
        Ok(resp) => match resp.text().await {
          Ok(text) => text,
          Err(e) => {
            return Err(ErrorData::internal_error(
              format!("failed to read {url}: {e}", url = input.url),
              None,
            ));
          }
        },
        Err(e) => {
          return Err(ErrorData::internal_error(
            format!("failed to connect to {url}: {e}", url = input.url),
            None,
          ));
        }
      }
    };

    // Try using the readability crate to extract content
    if let Ok(Some(article)) = Readability::new(
      &html,
      Some(&input.url),
      Some(ReadabilityOptions::builder().output_markdown(true).build()),
    )
    .map_err(|e| {
      format!("Failed extracting article from '{}': {}", input.url, e)
    })
    .map(|article| article.parse())
    {
      if let Some(markdown) = article.markdown_content {
        return Ok(Json(FetchArticleOutput { content: markdown }));
      } else if let Some(text) = article.text_content {
        return Ok(Json(FetchArticleOutput { content: text }));
      }
    }

    // Fallback to HTML parsing
    let document = Html::parse_document(&html);

    // Try common content selectors
    let mut text = String::new();
    let mut found = false;

    for selector in selectors::CONTENT_SELECTORS.iter() {
      if let Some(element) = document.select(selector).next() {
        let cleaned = strip_html(&element.html());
        if !cleaned.trim().is_empty() && cleaned.len() > 100 {
          text = cleaned;
          found = true;
          break;
        }
      }
    }

    if !found {
      // Fallback: extract from body
      if let Some(body) = document.select(&selectors::BODY_SELECTOR).next() {
        text = strip_html(&body.html());
      } else {
        text = strip_html(&html);
      }
    }

    // Clean up whitespace
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");

    if text.trim().is_empty() {
      return Err(ErrorData::internal_error(
        format!("could not extract any content from {url}", url = input.url),
        None,
      ));
    }

    Ok(Json(FetchArticleOutput { content: text }))
  }
}

/// Strip HTML tags and return plain text.
fn strip_html(html: &str) -> String {
  let fragment = Html::parse_fragment(html);
  fragment.root_element().text().collect::<String>()
}

#[cfg(test)]
mod tests {
  use super::*;
  use rmcp::model::ErrorCode;
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn make_server() -> RssServer {
    RssServer::new().expect("server construction")
  }

  // --- strip_html (no HTTP needed) ---

  #[test]
  fn test_strip_html_simple() {
    let input = "<p>Hello <b>world</b></p>";
    let output = strip_html(input);
    assert!(output.contains("Hello"));
    assert!(output.contains("world"));
  }

  #[test]
  fn test_strip_html_complex() {
    let input = "<div class=\"article\"><h1>Title</h1><p>Some text with <a href=\"#\">links</a> and <span>spans</span>.</p></div>";
    let output = strip_html(input);
    assert!(output.contains("Title"));
    assert!(output.contains("Some text with"));
    assert!(output.contains("links"));
    assert!(output.contains("spans"));
  }

  // --- get_articles ---

  #[tokio::test]
  async fn test_get_articles_basic() {
    let server = make_server();
    let mock_server = MockServer::start().await;

    let rss = r#"<?xml version="1.0"?>
    <rss version="2.0">
      <channel>
        <title>Test</title>
        <item><title>One</title><link>https://example.com/one</link><guid>guid-one</guid><description>Summary for one</description></item>
        <item><title>Two</title><link>https://example.com/two</link><guid>guid-two</guid><description>Summary for two</description></item>
      </channel>
    </rss>"#;

    Mock::given(wiremock::matchers::path("/feed.xml"))
      .respond_with(ResponseTemplate::new(200).set_body_string(rss))
      .mount(&mock_server)
      .await;

    let input = GetArticlesInput {
      feeds: vec![format!("{}/feed.xml", mock_server.uri())],
      time_from: None,
    };
    let result = server
      .get_articles(Parameters(input))
      .await
      .expect("get_articles should succeed");
    let articles = &result.0.articles;

    assert_eq!(articles.len(), 2);
    assert_eq!(articles[0].link, "https://example.com/one");
    assert_eq!(articles[0].title, "One");
    assert_eq!(articles[0].id, "guid-one");
    assert_eq!(articles[0].description, Some("Summary for one".to_string()));
    assert_eq!(articles[1].link, "https://example.com/two");
    assert_eq!(articles[1].title, "Two");
    assert_eq!(articles[1].id, "guid-two");
  }

  #[tokio::test]
  async fn test_get_articles_time_filter() {
    let server = make_server();
    let mock_server = MockServer::start().await;

    let rss = r#"<?xml version="1.0"?>
    <rss version="2.0">
      <channel>
        <title>Test</title>
        <item><title>Old</title><link>https://example.com/old</link><guid>guid-old</guid><pubDate>Mon, 01 Jan 2024 00:00:00 +0000</pubDate></item>
        <item><title>Recent</title><link>https://example.com/recent</link><guid>guid-recent</guid><pubDate>Fri, 19 Jul 2026 12:00:00 +0000</pubDate></item>
        <item><title>No Date</title><link>https://example.com/nodeate</link><guid>guid-nodeate</guid></item>
      </channel>
    </rss>"#;

    Mock::given(wiremock::matchers::path("/feed.xml"))
      .respond_with(ResponseTemplate::new(200).set_body_string(rss))
      .mount(&mock_server)
      .await;

    let input = GetArticlesInput {
      feeds: vec![format!("{}/feed.xml", mock_server.uri())],
      time_from: Some("2026-01-01T00:00:00+00:00".to_string()),
    };
    let result = server
      .get_articles(Parameters(input))
      .await
      .expect("get_articles should succeed");
    let articles = &result.0.articles;

    // "Old" is before the filter date — excluded
    assert!(
      articles
        .iter()
        .find(|a| a.link == "https://example.com/old")
        .is_none()
    );
    // "Recent" is after the filter date — included
    assert!(
      articles
        .iter()
        .find(|a| a.link == "https://example.com/recent")
        .is_some()
    );
    // "No Date" has no pubDate — included regardless of filter
    assert!(
      articles
        .iter()
        .find(|a| a.link == "https://example.com/nodeate")
        .is_some()
    );
    // All included articles should have correct metadata
    let recent = articles
      .iter()
      .find(|a| a.link == "https://example.com/recent")
      .unwrap();
    assert_eq!(recent.title, "Recent");
    assert_eq!(recent.id, "guid-recent");
  }

  #[tokio::test]
  async fn test_get_articles_dedup() {
    let server = make_server();
    let mock_server = MockServer::start().await;

    let rss = r#"<?xml version="1.0"?>
    <rss version="2.0">
      <channel>
        <title>Test</title>
        <item><title>A</title><link>https://example.com/a</link><guid>guid-a</guid></item>
        <item><title>B</title><link>https://example.com/b</link><guid>guid-b</guid></item>
      </channel>
    </rss>"#;

    Mock::given(wiremock::matchers::path("/feed1.xml"))
      .respond_with(ResponseTemplate::new(200).set_body_string(rss))
      .mount(&mock_server)
      .await;

    Mock::given(wiremock::matchers::path("/feed2.xml"))
      .respond_with(ResponseTemplate::new(200).set_body_string(rss))
      .mount(&mock_server)
      .await;

    let input = GetArticlesInput {
      feeds: vec![
        format!("{}/feed1.xml", mock_server.uri()),
        format!("{}/feed2.xml", mock_server.uri()),
      ],
      time_from: None,
    };
    let result = server
      .get_articles(Parameters(input))
      .await
      .expect("get_articles should succeed");

    // Each feed has 2 items, but they're the same URLs — should deduplicate to 2
    assert_eq!(result.0.articles.len(), 2);
  }

  #[tokio::test]
  async fn test_get_articles_invalid_xml() {
    let server = make_server();
    let mock_server = MockServer::start().await;

    Mock::given(wiremock::matchers::path("/bad.xml"))
      .respond_with(
        ResponseTemplate::new(200).set_body_string("not xml at all {{{"),
      )
      .mount(&mock_server)
      .await;

    let input = GetArticlesInput {
      feeds: vec![format!("{}/bad.xml", mock_server.uri())],
      time_from: None,
    };
    let result = server.get_articles(Parameters(input)).await;
    let Err(error) = result else {
      panic!("get_articles should fail on unparsable XML");
    };
    assert_eq!(error.code, ErrorCode::INTERNAL_ERROR);
    assert!(!error.message.is_empty());
  }

  #[tokio::test]
  async fn test_get_articles_empty_feeds() {
    let server = make_server();
    let input = GetArticlesInput {
      feeds: vec![],
      time_from: None,
    };
    let result = server.get_articles(Parameters(input)).await;
    let Err(error) = result else {
      panic!("get_articles should reject an empty feed list");
    };
    assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
  }

  // --- fetch_article ---

  #[tokio::test]
  async fn test_fetch_article_article_selector() {
    let server = make_server();
    let mock_server = MockServer::start().await;

    let html = r#"<html><body>
      <nav>Navigation should be completely ignored and excluded from the output</nav>
      <article><h1>Article Title</h1><p>This is the main article content that should be extracted from the page. It contains enough text to pass the length threshold check and should be properly selected over the body element and all other content on the page.</p></article>
      <footer>Footer should be completely ignored and excluded from the output as well</footer>
    </body></html>"#;

    Mock::given(wiremock::matchers::path("/article.html"))
      .respond_with(ResponseTemplate::new(200).set_body_string(html))
      .mount(&mock_server)
      .await;

    let input = FetchArticleInput {
      url: format!("{}/article.html", mock_server.uri()),
    };
    let result = server
      .fetch_article(Parameters(input))
      .await
      .expect("fetch_article should succeed");

    assert!(result.0.content.contains("Article Title"));
    assert!(result.0.content.contains("main article content"));
    assert!(!result.0.content.contains("Navigation"));
    assert!(!result.0.content.contains("Footer"));
  }

  #[tokio::test]
  async fn test_fetch_article_class_selector() {
    let server = make_server();
    let mock_server = MockServer::start().await;

    let html = r#"<html><body>
      <header>Header content should be completely ignored and not included in the extracted text</header>
      <div class="entry-content"><h1>Blog Post</h1><p>Blog post body text here. This is a longer paragraph that contains enough text to pass the length threshold and will be properly extracted by the selector matching logic.</p></div>
    </body></html>"#;

    Mock::given(wiremock::matchers::path("/blog.html"))
      .respond_with(ResponseTemplate::new(200).set_body_string(html))
      .mount(&mock_server)
      .await;

    let input = FetchArticleInput {
      url: format!("{}/blog.html", mock_server.uri()),
    };
    let result = server
      .fetch_article(Parameters(input))
      .await
      .expect("fetch_article should succeed");

    assert!(result.0.content.contains("Blog Post"));
    assert!(result.0.content.contains("Blog post body text"));
    assert!(!result.0.content.contains("Header content"));
  }

  #[tokio::test]
  async fn test_fetch_article_fallback() {
    let server = make_server();
    let mock_server = MockServer::start().await;

    // No article, main, or recognized class — should fall back to body
    let html = r#"<html><body>
      <div><p>This is all there is. Just some plain content.</p></div>
    </body></html>"#;

    Mock::given(wiremock::matchers::path("/plain.html"))
      .respond_with(ResponseTemplate::new(200).set_body_string(html))
      .mount(&mock_server)
      .await;

    let input = FetchArticleInput {
      url: format!("{}/plain.html", mock_server.uri()),
    };
    let result = server
      .fetch_article(Parameters(input))
      .await
      .expect("fetch_article should succeed");

    assert!(result.0.content.contains("all there is"));
    assert!(result.0.content.contains("plain content"));
  }
}

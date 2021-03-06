/*
 * Copyright 2015-2016 Torrie Fischer <tdfischer@hackerbots.net>
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::collections::BTreeMap;
use std::collections::HashMap;
use hyper;
use rustc_serialize::json::Json;
use rustc_serialize::json;
use std::fmt;
use std::result;
use matrix::json as mjson;
use matrix::events;
use matrix::model;

#[derive(Debug)]
pub enum ClientError {
    Http(hyper::Error),
    UrlNotFound,
    Json(json::ParserError)
}

pub type Result<T = ()> = result::Result<T, ClientError>;

mod http {
    use rustc_serialize::json::Json;
    use hyper;
    use std::io::Read;
    use matrix::client::{Result,ClientError};

    pub fn json(http: hyper::client::RequestBuilder) -> Result<Json> {
        let mut response = String::new();
        http.send().map_err(|err|{
            ClientError::Http(err)
        }).and_then(|mut res|{
            match res.status  {
                hyper::status::StatusCode::Ok =>  {
                    res.read_to_string(&mut response).expect("Could not read response");
                    Json::from_str(response.trim()).map_err(|err|{
                        ClientError::Json(err)
                    })
                },
                _ => Err(ClientError::UrlNotFound)
            }
        })
    }
}

pub struct AsyncPoll {
    http: hyper::client::Client,
    url: hyper::Url
}

impl AsyncPoll {
    pub fn send(self) -> Result<Vec<events::Event>> {
        http::json(self.http.get(self.url)).and_then(|json| {
            let mut ret: Vec<events::Event> = vec![];
            let events = mjson::array(&json, "chunk");
            for ref evt in events {
                trace!("<<< {}", evt);
                ret.push(events::Event::from_json(evt))
            }
            Ok(ret)
        })
    }
}

#[derive(Clone)]
pub struct AccessToken {
    access: String,
    refresh: String
}

pub struct Client {
    http: hyper::Client,
    token: Option<AccessToken>,
    next_id: u32,
    baseurl: String,
    pub uid: Option<model::UserID>
}

impl fmt::Debug for Client {
    fn fmt(&self, _: &mut fmt::Formatter) -> fmt::Result {
        Ok(())
    }
}

impl Client {
    pub fn new(baseurl: &str) -> Self {
        if !baseurl.starts_with("https") {
            warn!("YOU ARE CONNECTING TO A MATRIX SERVER WITHOUT SSL");
        }
        let mut http  = hyper::Client::new();
        http.set_redirect_policy(hyper::client::RedirectPolicy::FollowAll);
        Client {
            http: http,
            token: None,
            next_id: 0,
            baseurl: baseurl.to_string(),
            uid: None
        }
    }

    pub fn login(&mut self, username: &str, password: &str) -> Result {
        let mut d = BTreeMap::new();
        d.insert("user".to_string(), Json::String(username.to_string()));
        d.insert("password".to_string(), Json::String(password.to_string()));
        d.insert("type".to_string(), Json::String("m.login.password".to_string()));
        debug!("Logging in to matrix");
        http::json(self.http.post(self.url("login", &HashMap::new()))
            .body(Json::Object(d).to_string().trim()))
            .and_then(|js| {
                let obj = js.as_object().unwrap();
                self.token = Some(AccessToken {
                    access: obj.get("access_token").unwrap().as_string().unwrap().to_string(),
                    refresh: obj.get("refresh_token").unwrap().as_string().unwrap().to_string()
                });
                let url = hyper::Url::parse(self.baseurl.trim()).unwrap();
                let domain = url.host().unwrap().serialize();
                self.uid = Some(model::UserID::from_str(format!("@{}:{}", username, domain).trim()));
                Ok(())
            })
    }

    fn url(&self, endpoint: &str, args: &HashMap<&str, &str>) -> hyper::Url {
        let mut ret = self.baseurl.clone();
        ret.push_str(endpoint);
        ret.push_str("?");
        match self.token {
            None => (),
            Some(ref token) => {
                ret.push_str("access_token=");
                ret.push_str(token.access.trim());
                ret.push_str("&");
            }
        }
        for (name, value) in args {
            ret.push_str(name);
            ret.push_str("=");
            ret.push_str(value);
            ret.push_str("&");
        }
        hyper::Url::parse(ret.trim()).unwrap()
    }

    pub fn poll_async(&mut self) -> AsyncPoll {
        let url = self.url("events", &HashMap::new());
        let mut http = hyper::client::Client::new();
        http.set_redirect_policy(hyper::client::RedirectPolicy::FollowAll);
        AsyncPoll {
            http: http,
            url: url
        }
    }

    pub fn send(&mut self, evt: events::EventData) -> Result<model::EventID> {
        self.next_id += 1;
        match evt {
            events::EventData::Room(ref id, _) => {
                let url = self.url(format!("rooms/{}/send/{}/{}",
                                           id,
                                           evt.type_str(),
                                           self.next_id).trim(),
                                   &HashMap::new());
                trace!("Sending events to {:?}", url);
                // FIXME: This seems needed since hyper will pool HTTP client
                // connections for pipelining. Sometimes the server will close
                // the pooled connection and everything will catch on fire here.
                let mut http = hyper::client::Client::new();
                http.set_redirect_policy(hyper::client::RedirectPolicy::FollowAll);
                http::json(http.put(url).body(format!("{}", evt.to_json()).trim()))
            },
            _ => panic!("Don't know where to send {}", evt.to_json())
        }.and_then(|response| {
            trace!(">>> {} {:?}", evt.to_json(), response);
            Ok(model::EventID::from_str(mjson::string(&response, "event_id")))
        })
    }

    pub fn sync(&mut self) -> Result<Vec<events::Event>> {
        debug!("Syncing...");
        let mut args = HashMap::new();
        args.insert("limit", "0");
        let url = self.url("initialSync", &args);
        http::json(self.http.get(url)).and_then(|js| {
            let rooms = mjson::array(&js, "rooms");
            let mut ret: Vec<events::Event> = vec![];
            for ref r in rooms {
                let room_state = mjson::array(r, "state");
                for ref evt in room_state {
                    trace!("<<< {}", evt);
                    // FIXME: It'd be nice to return to the previous
                    // callback-based mechanism to avoid memory bloat
                    ret.push(events::Event::from_json(evt));
                };
            }
            ret.push(events::Event {
                data: events::EventData::EndOfSync,
                id: None
            });
            Ok(ret)
        })
    }
}

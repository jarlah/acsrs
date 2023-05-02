/*
 * Copyright (C) 2023 Guillaume Pellegrino
 * This file is part of acsrs <https://github.com/guillaumepellegrino/acsrs>.
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */
use crate::db;
use crate::soap;
use crate::utils;
use base64::Engine;
use eyre::{eyre, Result};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

#[derive(Debug, Clone)]
pub struct Connreq {
    pub url: String,
    pub username: String,
    pub password: String,
}

#[derive(Debug)]
pub struct Transfer {
    pub msg: soap::Envelope,
    pub observer: Option<mpsc::Sender<soap::Envelope>>,
}

#[derive(Debug)]
#[allow(clippy::upper_case_acronyms)]
pub struct CPE {
    pub device_id: soap::DeviceId,
    pub connreq: Connreq,
    pub baseurl: String,

    /** Number of TR069Session opened for this CPE + 1 */
    tr069_session_refcount: Arc<()>,

    /** Number of CPEController running for this CPE + 1 */
    cpe_controllers_refcount: Arc<()>,

    transfers_tx: flume::Sender<Transfer>,
    transfers_rx: flume::Receiver<Transfer>,
}

pub struct CPEController {
    cpe: Arc<RwLock<CPE>>,
    transfers_tx: flume::Sender<Transfer>,
    _refcount: Arc<()>,
}

#[derive(Debug, Default)]
pub struct Acs {
    pub config: db::AcsConfig,
    pub basicauth: String,
    pub cpe_list: HashMap<String, Arc<RwLock<CPE>>>,
    pub acsdir: std::path::PathBuf,
}

impl Transfer {
    pub fn new() -> Self {
        Self {
            msg: soap::Envelope::new("1"),
            observer: None,
        }
    }

    pub fn rxchannel(&mut self) -> mpsc::Receiver<soap::Envelope> {
        let (tx, rx) = mpsc::channel(1);
        self.observer = Some(tx);
        rx
    }
}

impl Default for Connreq {
    fn default() -> Self {
        Self {
            url: String::from(""),
            username: String::from("acsrs"),
            password: utils::random_password(),
        }
    }
}

impl Connreq {
    pub async fn send(&self) -> Result<()> {
        let client = reqwest::Client::new();

        // Step 1:  Get the auth header
        let res = client.get(&self.url).send().await?;
        let headers = res.headers();
        let wwwauth = headers
            .get("www-authenticate")
            .ok_or(eyre!("connreq reply without auth header"))?
            .to_str()?;

        // Step 2:  Given the auth header, sign the digest for the real req.
        let context = digest_auth::AuthContext::new(&self.username, &self.password, "/");
        let mut prompt = digest_auth::parse(wwwauth)?;
        let answer = prompt.respond(&context)?.to_header_string();
        let response = client
            .get(&self.url)
            .header("Authorization", answer)
            .send()
            .await?;

        match response.status() {
            reqwest::StatusCode::OK => Ok(()),
            _ => Err(eyre!("ConnectionRequest: Authentication failed")),
        }
    }
}

impl Default for CPE {
    fn default() -> Self {
        let (tx, rx) = flume::unbounded();
        Self {
            device_id: soap::DeviceId::default(),
            connreq: Connreq::default(),
            baseurl: String::new(),
            tr069_session_refcount: Arc::new(()),
            cpe_controllers_refcount: Arc::new(()),
            transfers_tx: tx,
            transfers_rx: rx,
        }
    }
}

impl CPE {
    pub fn tr069_session_opened(&self) -> bool {
        Arc::strong_count(&self.tr069_session_refcount) > 1
    }

    pub fn cpe_controller_running(&self) -> bool {
        Arc::strong_count(&self.cpe_controllers_refcount) > 1
    }

    pub fn get_tr069_session_refcount(&self) -> Arc<()> {
        self.tr069_session_refcount.clone()
    }
    pub fn get_transfers_rx(&self) -> flume::Receiver<Transfer> {
        self.transfers_rx.clone()
    }
}

impl CPEController {
    pub async fn new(cpelock: Arc<RwLock<CPE>>) -> Self {
        let cpe = cpelock.read().await;
        Self {
            cpe: cpelock.clone(),
            transfers_tx: cpe.transfers_tx.clone(),
            _refcount: cpe.cpe_controllers_refcount.clone(),
        }
    }

    pub async fn add_transfer(&self, transfer: Transfer) -> Result<()> {
        self.transfers_tx.send_async(transfer).await?;

        let cpe = self.cpe.read().await;
        if !cpe.tr069_session_opened() {
            let connreq = cpe.connreq.clone();
            drop(cpe);

            // Send the ConnectionRequest to CPE
            println!("Send ConnectionRequest to {}", connreq.url);
            connreq.send().await?;
            println!("ConnectionRequest was acknowledged");
        }

        Ok(())
    }
}

impl Acs {
    pub fn new(acsdir: &std::path::Path) -> Self {
        let mut acs = Self::default();
        acs.config = db::AcsConfig {
            hostname: String::new(),
            username: utils::random_password(),
            password: utils::random_password(),
            autocert: true,
            unsecure_address: String::from("[::0]:8080"),
            identity_password: String::from("ACSRS"),
            secure_address: String::from("[::0]:8443"),
            management_address: String::from("127.0.0.1:8000"),
        };
        acs.basicauth = Self::basicauth(&acs.config.username, &acs.config.password);
        acs.acsdir = acsdir.to_path_buf();
        acs
    }

    fn basicauth(username: &str, password: &str) -> String {
        let token = format!("{}:{}", username, password);
        let token64 = base64::engine::general_purpose::STANDARD.encode(token);
        format!("Basic {}", token64)
    }

    pub async fn save(&self) -> Result<()> {
        let savefile = self.acsdir.join("config.toml");
        println!("Save ACS config at {:?}", savefile);

        let mut db = db::Acs {
            config: self.config.clone(),
            ..Default::default()
        };

        for (sn, cpe) in &self.cpe_list {
            let cpe = cpe.read().await;
            let elem = db::CPE {
                serial_number: sn.clone(),
                url: cpe.connreq.url.clone(),
                username: cpe.connreq.username.clone(),
                password: cpe.connreq.password.clone(),
            };
            db.cpe.push(elem);
        }

        db.save(&savefile)
    }

    pub async fn restore(acsdir: &std::path::Path) -> Result<Acs> {
        let savefile = acsdir.join("config.toml");
        let db = db::Acs::restore(&savefile)?;
        let mut acs = Self::default();
        acs.config = db.config.clone();
        acs.basicauth = Acs::basicauth(&acs.config.username, &acs.config.password);
        acs.acsdir = acsdir.to_path_buf();

        for elem in &db.cpe {
            let mut cpe = CPE::default();
            cpe.device_id.serial_number = elem.serial_number.clone();
            cpe.connreq.url = elem.url.clone();
            cpe.connreq.username = elem.username.clone();
            cpe.connreq.password = elem.password.clone();
            acs.cpe_list
                .insert(elem.serial_number.clone(), Arc::new(RwLock::new(cpe)));
        }

        Ok(acs)
    }

    pub fn print_config(&self, hostname: &str) {
        let hostname = match hostname.contains(":") {
            true => {
                format!("[{}]", hostname)
            }
            false => {
                format!("{}", hostname)
            }
        };

        println!();
        let addr: SocketAddr = self.config.secure_address.parse().unwrap();
        println!("For secure connections, please ensure your CPEs are configured with:");
        println!(
            "Device.ManagementServer.URL=\"https://{}:{}/cwmpWeb/CPEMgt\"",
            hostname,
            addr.port()
        );
        println!(
            "Device.ManagementServer.Username=\"{}\"",
            self.config.username
        );
        println!(
            "Device.ManagementServer.Password=\"{}\"",
            self.config.password
        );
        println!();
        let addr: SocketAddr = self.config.unsecure_address.parse().unwrap();
        println!("For unsecure connections, please ensure your CPEs are configured with:");
        println!(
            "Device.ManagementServer.URL=\"http://{}:{}/cwmpWeb/CPEMgt\"",
            hostname,
            addr.port()
        );
        println!(
            "Device.ManagementServer.Username=\"{}\"",
            self.config.username
        );
        println!(
            "Device.ManagementServer.Password=\"{}\"",
            self.config.password
        );
        println!();
    }
}

#[tokio::test]
async fn test_acs_save_restore() {
    let tmp = std::path::PathBuf::from("/tmp");
    let mut acs = Acs::new(&tmp);

    let mut cpe1 = CPE::default();
    cpe1.connreq.url = String::from("http://192.168.1.X:7547/CPE1");
    acs.cpe_list
        .insert("CPE1_SN".to_string(), Arc::new(RwLock::new(cpe1)));

    let mut cpe2 = CPE::default();
    cpe2.connreq.url = String::from("http://192.168.1.X:7547/CPE2");
    acs.cpe_list
        .insert("CPE2_SN".to_string(), Arc::new(RwLock::new(cpe2)));

    acs.save().await.unwrap();

    let restored = Acs::restore(&tmp).await.unwrap();
    assert_eq!(&restored.config.username, &acs.config.username);
    assert_eq!(&restored.config.password, &acs.config.password);
    assert_eq!(&restored.basicauth, &acs.basicauth);
    assert_eq!(
        &restored.cpe_list["CPE1_SN"].read().await.connreq.url,
        "http://192.168.1.X:7547/CPE1"
    );
    assert_eq!(
        &restored.cpe_list["CPE2_SN"].read().await.connreq.url,
        "http://192.168.1.X:7547/CPE2"
    );
}

use super::AppCore;
use crate::profile_cache::{download_nwc_icon, nwc_icon_file_url};
use crate::updates::{AsyncMsg, CoreMsg};

impl AppCore {
    pub(super) fn hydrate_nwc_icon_urls(&mut self) {
        for connection in &mut self.state.nwc.connections {
            connection.icon_display_url = connection
                .icon_url
                .as_deref()
                .and_then(|url| nwc_icon_file_url(&self.cache_dir, url));
        }
        if let Some(request) = self.state.nwa.request.as_ref() {
            self.state.nwa.icon_display_url = request
                .icon_url
                .as_deref()
                .and_then(|url| nwc_icon_file_url(&self.cache_dir, url));
        }
    }

    pub(super) fn prefetch_nwc_icons(&mut self) {
        let mut urls = self
            .state
            .nwc
            .connections
            .iter()
            .filter_map(|connection| connection.icon_url.clone())
            .collect::<Vec<_>>();
        if let Some(url) = self
            .state
            .nwa
            .request
            .as_ref()
            .and_then(|request| request.icon_url.clone())
        {
            urls.push(url);
        }
        urls.sort();
        urls.dedup();
        for url in urls {
            self.prefetch_nwc_icon(url);
        }
    }

    pub(super) fn prefetch_nwc_icon(&mut self, remote_url: String) {
        if nwc_icon_file_url(&self.cache_dir, &remote_url).is_some()
            || !self.nwc_icon_downloads.insert(remote_url.clone())
        {
            self.refresh_nwc_icon_url(&remote_url);
            return;
        }

        let tx = self.tx.clone();
        let cache_dir = self.cache_dir.clone();
        let semaphore = self.profile_picture_download_semaphore.clone();
        self.rt.spawn(async move {
            let failed_url = remote_url.clone();
            let message =
                match download_nwc_icon(reqwest::Client::new(), cache_dir, remote_url, semaphore)
                    .await
                {
                    Ok(remote_url) => AsyncMsg::NwcIconCached { remote_url },
                    Err(_) => AsyncMsg::NwcIconCacheFailed {
                        remote_url: failed_url,
                    },
                };
            let _ = tx.send(CoreMsg::Async(message));
        });
    }

    pub(super) fn finish_nwc_icon_cache(&mut self, remote_url: String, succeeded: bool) {
        self.nwc_icon_downloads.remove(&remote_url);
        if succeeded {
            self.refresh_nwc_icon_url(&remote_url);
        }
    }

    pub(super) fn nwc_icon_display_url(&self, remote_url: Option<&str>) -> Option<String> {
        remote_url.and_then(|url| nwc_icon_file_url(&self.cache_dir, url))
    }

    fn refresh_nwc_icon_url(&mut self, remote_url: &str) {
        let Some(file_url) = nwc_icon_file_url(&self.cache_dir, remote_url) else {
            return;
        };
        for connection in &mut self.state.nwc.connections {
            if connection.icon_url.as_deref() == Some(remote_url) {
                connection.icon_display_url = Some(file_url.clone());
            }
        }
        if let Some(request) = self.state.nwa.request.as_ref() {
            if request.icon_url.as_deref() == Some(remote_url) {
                self.state.nwa.icon_display_url = Some(file_url);
            }
        }
    }
}

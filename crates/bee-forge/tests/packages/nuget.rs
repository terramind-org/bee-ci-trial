//! NuGet v3: service index, push (multipart and raw), flat container, registration, search.

use bytes::Bytes;
use serde_json::json;

use super::support::*;

fn nupkg(id: &str, version: &str) -> Vec<u8> {
    let nuspec = format!(
        r#"<?xml version="1.0"?><package xmlns="http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd"><metadata><id>{id}</id><version>{version}</version><authors>Ada</authors><description>A library</description><repository type="git" url="https://beehive.test/acme/app"/><dependencies><group targetFramework="net8.0"><dependency id="Newtonsoft.Json" version="13.0.1"/></group></dependencies></metadata></package>"#
    );
    zip(
        &[
            (&format!("{id}.nuspec"), nuspec.as_bytes()),
            ("lib/net8.0/a.dll", b"MZ-fake-dll"),
            ("[Content_Types].xml", b"<Types/>"),
        ],
        &format!("{id}.nuspec"),
    )
}

fn multipart(body: &[u8]) -> (String, Vec<u8>) {
    let b = "----beeboundary";
    let mut out = format!("--{b}\r\nContent-Disposition: form-data; name=\"package\"; filename=\"package.nupkg\"\r\nContent-Type: application/octet-stream\r\n\r\n").into_bytes();
    out.extend_from_slice(body);
    out.extend_from_slice(format!("\r\n--{b}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={b}"), out)
}

#[tokio::test]
async fn push_registration_and_flat_container() {
    let w = world().await;
    let tok = w.tok("owner");
    let idx = w
        .http(rq(
            "GET",
            "/nuget/acme/v3/index.json",
            None,
            &[],
            Bytes::new(),
        ))
        .await;
    let idx = body_json(&idx);
    let kinds: Vec<&str> = idx["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["@type"].as_str().unwrap())
        .collect();
    for k in [
        "PackagePublish/2.0.0",
        "SearchQueryService",
        "RegistrationsBaseUrl",
        "PackageBaseAddress/3.0.0",
    ] {
        assert!(kinds.contains(&k), "{k}");
    }
    let publish_url = idx["resources"][0]["@id"]
        .as_str()
        .unwrap()
        .replace("https://beehive.test", "");
    assert_eq!(publish_url, "/nuget/acme/api/v2/package");

    let pkg = nupkg("Acme.Lib", "1.0");
    let (ct, body) = multipart(&pkg);
    let r = w
        .http(rq(
            "PUT",
            &publish_url,
            Some(&tok),
            &[("content-type", &ct), ("x-nuget-apikey", &tok)],
            body,
        ))
        .await;
    assert_eq!(r.status(), 201, "{}", body_str(&r));
    // The same normalized version cannot be pushed again, however it is spelled.
    let again = nupkg("Acme.Lib", "1.0.0.0");
    let r = w
        .http(rq("PUT", &publish_url, Some(&tok), &[], again))
        .await;
    assert_eq!(r.status(), 409);
    let beta = nupkg("Acme.Lib", "2.0.0-Beta.1");
    assert_eq!(
        w.http(rq("PUT", &publish_url, Some(&tok), &[], beta))
            .await
            .status(),
        201
    );
    // Flat container: lower-case ids, normalized versions, ascending.
    let reader = w.tok("reader");
    let f = w
        .http(rq(
            "GET",
            "/nuget/acme/v3/flatcontainer/acme.lib/index.json",
            Some(&reader),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(body_json(&f)["versions"], json!(["1.0.0", "2.0.0-beta.1"]));
    let n = w
        .http(rq(
            "GET",
            "/nuget/acme/v3/flatcontainer/acme.lib/1.0.0/acme.lib.1.0.0.nupkg",
            Some(&reader),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(n.status(), 200);
    assert_eq!(n.body().as_ref(), pkg.as_slice());
    // Requesting "1.0" finds the normalized version.
    let n = w
        .http(rq(
            "GET",
            "/nuget/acme/v3/flatcontainer/Acme.Lib/1.0/acme.lib.1.0.0.nupkg",
            Some(&reader),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(n.status(), 200);
    let spec = w
        .http(rq(
            "GET",
            "/nuget/acme/v3/flatcontainer/acme.lib/1.0.0/acme.lib.nuspec",
            Some(&reader),
            &[],
            Bytes::new(),
        ))
        .await;
    assert!(body_str(&spec).contains("<id>Acme.Lib</id>"));
    // Registration: one page with dependency groups.
    let reg = body_json(
        &w.http(rq(
            "GET",
            "/nuget/acme/v3/registration/acme.lib/index.json",
            Some(&reader),
            &[],
            Bytes::new(),
        ))
        .await,
    );
    let items = reg["items"][0]["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(reg["items"][0]["lower"], "1.0.0");
    assert_eq!(reg["items"][0]["upper"], "2.0.0-beta.1");
    let entry = &items[0]["catalogEntry"];
    assert_eq!(entry["id"], "Acme.Lib");
    assert_eq!(entry["version"], "1.0.0");
    assert_eq!(entry["dependencyGroups"][0]["targetFramework"], "net8.0");
    assert_eq!(
        entry["dependencyGroups"][0]["dependencies"][0]["id"],
        "Newtonsoft.Json"
    );
    assert!(
        items[0]["packageContent"]
            .as_str()
            .unwrap()
            .ends_with("/flatcontainer/acme.lib/1.0.0/acme.lib.1.0.0.nupkg")
    );
    let leaf = w
        .http(rq(
            "GET",
            "/nuget/acme/v3/registration/acme.lib/1.0.0.json",
            Some(&reader),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(body_json(&leaf)["listed"], true);
    // Search: prerelease hidden by default.
    let s = body_json(
        &w.http(rq(
            "GET",
            "/nuget/acme/v3/query?q=acme",
            Some(&reader),
            &[],
            Bytes::new(),
        ))
        .await,
    );
    assert_eq!(s["totalHits"], 1);
    assert_eq!(s["data"][0]["version"], "1.0.0");
    assert_eq!(s["data"][0]["totalDownloads"], 2);
    // Auth: anonymous and outsiders cannot read the private (repo-linked) package.
    assert_eq!(
        w.http(rq(
            "GET",
            "/nuget/acme/v3/flatcontainer/acme.lib/index.json",
            None,
            &[],
            Bytes::new()
        ))
        .await
        .status(),
        401
    );
    assert_eq!(
        w.http(rq(
            "GET",
            "/nuget/acme/v3/flatcontainer/acme.lib/index.json",
            Some(&w.tok("outsider")),
            &[],
            Bytes::new()
        ))
        .await
        .status(),
        404
    );
    // Garbage is refused.
    assert_eq!(
        w.http(rq(
            "PUT",
            &publish_url,
            Some(&tok),
            &[],
            b"not a zip".to_vec()
        ))
        .await
        .status(),
        400
    );
    // Delete (unlist) a version.
    let d = w
        .http(rq(
            "DELETE",
            "/nuget/acme/api/v2/package/Acme.Lib/2.0.0-beta.1",
            Some(&tok),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(d.status(), 204);
    let f = w
        .http(rq(
            "GET",
            "/nuget/acme/v3/flatcontainer/acme.lib/index.json",
            Some(&reader),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(body_json(&f)["versions"], json!(["1.0.0"]));
}

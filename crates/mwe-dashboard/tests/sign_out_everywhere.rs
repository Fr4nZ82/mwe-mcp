// SPDX-License-Identifier: AGPL-3.0-or-later
//! Signing out ends every session the person has open, not only the one
//! doing the signing out — and it leaves the browser with no session at
//! all. Changing the password does the same and keeps this one alive.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{body_string, extract_cookie_value, extract_set_cookie, make_app, send};

const PASSWORD: &str = "correct-horse-battery";

/// Create the admin and return the cookie the setup wizard mints.
async fn create_admin(app: &axum::Router) -> String {
    let response = send(
        app,
        Request::builder()
            .method("POST")
            .uri("/setup")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "email=alice@example.com&admin_id=alice\
                 &password={PASSWORD}&password_confirm={PASSWORD}"
            )))
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection(), "setup must sign us in");
    let cookie =
        extract_cookie_value(&extract_set_cookie(&response, "mwe_session").expect("cookie"));

    // Skip the profile wizard, so that from here on `/home` answers with
    // the dashboard rather than sending every session to `/welcome`.
    let response = send(
        app,
        Request::builder()
            .method("POST")
            .uri("/welcome")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("action=skip"))
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection(), "welcome skip");
    cookie
}

/// Sign in again — a second browser, a phone, anything holding its own
/// cookie for the same person.
async fn sign_in(app: &axum::Router) -> String {
    let response = send(
        app,
        Request::builder()
            .method("POST")
            .uri("/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "email=alice@example.com&password={PASSWORD}"
            )))
            .unwrap(),
    )
    .await;
    assert!(response.status().is_redirection(), "login must sign us in");
    extract_cookie_value(&extract_set_cookie(&response, "mwe_session").expect("cookie"))
}

/// Does this cookie still open the dashboard?
async fn still_signed_in(app: &axum::Router, cookie: &str) -> bool {
    let response = send(
        app,
        Request::builder()
            .uri("/home")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    response.status() == StatusCode::OK
}

async fn post_logout(app: &axum::Router, cookie: &str) -> axum::http::Response<Body> {
    send(
        app,
        Request::builder()
            .method("POST")
            .uri("/logout")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

/// The point of the whole thing: a phone left on a train is signed out
/// from the laptop. The other device's cookie is refused — not left to
/// expire on its own.
#[tokio::test]
async fn signing_out_ends_the_session_on_the_other_device_too() {
    let (app, _dir) = make_app().await;
    let laptop = create_admin(&app).await;
    let phone = sign_in(&app).await;
    assert!(
        still_signed_in(&app, &phone).await,
        "premise: both are live"
    );

    let response = post_logout(&app, &laptop).await;
    assert!(response.status().is_redirection());

    assert!(
        !still_signed_in(&app, &phone).await,
        "signing out on one device must end the session on the other, not \
         leave it signed in until it expires"
    );
}

/// Signing out must not hand the browser a session back. The sliding
/// refresher appends its own `Set-Cookie` after the handler's, and a
/// browser keeps the last one it is given — so a response carrying a
/// signed JWT after the cleared cookie is a sign-out that did nothing.
#[tokio::test]
async fn signing_out_leaves_the_browser_no_session_cookie() {
    let (app, _dir) = make_app().await;
    let cookie = create_admin(&app).await;

    let response = post_logout(&app, &cookie).await;

    let mut seen = 0;
    for header in response.headers().get_all(header::SET_COOKIE) {
        let value = header.to_str().expect("ascii header");
        if !value.starts_with("mwe_session=") {
            continue;
        }
        seen += 1;
        assert!(
            value.starts_with("mwe_session=;"),
            "the sign-out response set a session cookie with a value in it: {value}"
        );
    }
    assert_eq!(
        seen, 1,
        "exactly one session cookie is set, and it is empty"
    );
}

/// A new password ends the sessions opened with the old one — and keeps
/// the one that changed it, which is what the Settings page promises.
#[tokio::test]
async fn changing_the_password_ends_the_other_sessions_and_keeps_this_one() {
    let (app, _dir) = make_app().await;
    let laptop = create_admin(&app).await;
    let phone = sign_in(&app).await;

    let response = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/settings/me")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, &laptop)
            .body(Body::from(format!(
                "current_password={PASSWORD}\
                 &new_password=brand-new-pw-2026!\
                 &new_password_confirm=brand-new-pw-2026!"
            )))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let refreshed =
        extract_cookie_value(&extract_set_cookie(&response, "mwe_session").expect("cookie"));
    let html = body_string(response).await;
    assert!(html.contains("Password updated"), "{html}");

    assert!(
        !still_signed_in(&app, &phone).await,
        "the sessions opened with the old password are over"
    );
    assert!(
        still_signed_in(&app, &refreshed).await,
        "the browser that changed the password stays signed in"
    );
}

/// The button is named after what it does.
///
/// One press ends every session this person has open, on every device. A
/// bare "Log out" reads as ending this browser only, which is the other
/// plausible behaviour and not the one the button has — so the label says
/// which, and the settings page names the button by that same label rather
/// than describing it in its own words.
#[tokio::test]
async fn the_button_and_the_settings_page_both_say_log_out_everywhere() {
    let (app, _dir) = make_app().await;
    let cookie = create_admin(&app).await;

    for uri in ["/home", "/settings/me"] {
        let response = send(
            &app,
            Request::builder()
                .uri(uri)
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        let html = body_string(response).await;
        assert!(
            html.contains("Log out everywhere"),
            "`{uri}` must name the button by what it does: {html}"
        );
        assert!(
            !html.contains(">Log out<"),
            "`{uri}` still offers a bare \"Log out\": {html}"
        );
    }
}

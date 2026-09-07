# Your account

**Nav: Settings.** `/dashboard/settings/me`

## Your password

**Current password**, **New password**, **Confirm new password**, then
**Update password**. Minimum 12 characters. Changing it ends every other
session you have open, on every device, and keeps you signed in here.

If you have forgotten it, the sign-in page offers *Forgot your password?* only
when the deployment has an outgoing mail server set up. The link it emails is
good for 30 minutes. Where there is no mail server, the page says so plainly
and tells you to ask your admin for a fresh invitation link instead.

Your sign-in itself lasts 60 minutes and slides: every page you open pushes it
out again, so it lapses only when you stop.

## Log out everywhere

The button beside your name in the top bar, on every page. It ends every
session you have open, everywhere. Use it when you have signed in on a device
you no longer have.

## Two-factor sign-in

**Manage two-factor authentication →** takes you to the second layer: a
time-based code from an authenticator app (Aegis, Google Authenticator,
1Password, and so on).

**Set up two-factor** shows a QR code and, under it, the same key in text for
an app that cannot scan. Add it, then type the six-digit code your app shows
and press **Verify and turn on**.

You are then shown **ten recovery codes**, once. Each works once, if you lose
your authenticator. Store them somewhere safe — they are not shown again. From
then on the page says two-factor is on and how many unused recovery codes you
have left, with **Generate new recovery codes** and **Turn off two-factor**.

At the next sign-in, after your password, you are asked for the six-digit code.
Lost your device? Type one of your recovery codes into the same field.

Your admin may require two-factor for everybody, in which case you are asked to
set it up before you can use the dashboard.

// Chromium through Caddy's forward_auth to stdlib:session, as a person
// signs in: script/session-browser.sh starts everything this talks to.
//
//   PROXY=<port> node check.mjs
//
// *.example.test resolves to loopback inside this browser only.

import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import http from 'node:http';
import path from 'node:path';

const { chromium } = await import(process.env.PLAYWRIGHT ? path.join(process.env.PLAYWRIGHT, 'index.mjs') : 'playwright');
const PROXY = process.env.PROXY;
const APP = `http://app.example.test:${PROXY}`;
const AUTH = `http://auth.example.test:${PROXY}`;

// depth: TOTP (RFC 6238, SHA-1), computed here as an authenticator app would

function base32(text) {
  const alphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567';
  let bits = 0, acc = 0; const out = [];
  for (const c of text) {
    acc = (acc << 5) | alphabet.indexOf(c); bits += 5;
    if (bits >= 8) { bits -= 8; out.push((acc >> bits) & 255); }
  }
  return Buffer.from(out);
}

function totp(secret, offset = 0) {
  const step = Math.floor(Date.now() / 1000 / 30) + offset;
  const msg = Buffer.alloc(8); msg.writeBigUInt64BE(BigInt(step));
  const h = crypto.createHmac('sha1', base32(secret)).update(msg).digest();
  const o = h[19] & 15;
  return String((h.readUInt32BE(o) & 0x7fffffff) % 1000000).padStart(6, '0');
}

// depth: the checks

let passed = 0;
function ok(what) { passed += 1; console.log(`ok ${passed} - ${what}`); }

const browser = await chromium.launch({
  executablePath: process.env.CHROMIUM || undefined,
  args: ['--host-resolver-rules=MAP *.example.test 127.0.0.1'],
});

async function signIn(context, user, password) {
  const page = await context.newPage();
  await page.goto(`${APP}/private?x=1`);
  assert.equal(new URL(page.url()).host, `auth.example.test:${PROXY}`);
  assert.equal(new URL(page.url()).searchParams.get('rd'), `http://app.example.test:${PROXY}/private?x=1`);
  ok(`${user}: an unsigned visit to the app lands on the login, carrying where it was going`);

  await page.fill('#username', user);
  await page.fill('#password', 'not-' + password);
  await page.click('button[type=submit]');
  assert.match(await page.textContent('[role=alert]'), /Wrong username or password/);
  ok(`${user}: a wrong password is refused on the page`);

  await page.fill('#username', user);
  await page.fill('#password', password);
  await page.click('button[type=submit]');
  assert.equal(new URL(page.url()).pathname, '/2fa');
  const secret = (await page.textContent('#totp-secret')).trim();
  assert.match(await page.getAttribute('#totp-uri', 'href'), new RegExp(`^otpauth://totp/Example:${user}\\?secret=${secret}&`));
  ok(`${user}: the password opens TOTP enrolment, with the otpauth URI`);

  await page.fill('#code', totp(secret));
  await page.click('button[type=submit]');
  assert.equal(page.url(), `${APP}/private?x=1`);
  return { page, secret };
}

// Alice: an admin.
const alice = await browser.newContext();
const { page, secret: aliceSecret } = await signIn(alice, 'alice', 'alice-pw');
assert.equal(await page.textContent('body'), 'user=alice groups=admins,staff path=/private');
ok('alice: the first code signs in, the browser goes back to the app, and the app is told who she is');

const cookies = await alice.cookies();
const c = cookies.find((k) => k.name === 'drt_session');
assert.ok(c, JSON.stringify(cookies));
assert.equal(c.domain, '.example.test');
assert.equal(c.httpOnly, true);
assert.equal(c.sameSite, 'Lax');
assert.equal(cookies.length, 1);
ok('alice: one cookie, for the parent domain, HttpOnly and SameSite=Lax');

let r = await page.goto(`${APP}/admin`);
assert.equal(r.status(), 200);
assert.equal(await page.textContent('body'), 'user=alice groups=admins,staff path=/admin');
ok('alice: /admin, gated on the admins group, admits her');

await page.goto(`${AUTH}/`);
assert.equal(await page.textContent('#user'), 'alice');
// The login pages run no script (their CSP says default-src 'none'), so
// the token is asked for here, with the cookie the browser holds, through
// the proxy as auth.example.test.
const token = await new Promise((resolve, reject) => {
  const req = http.request({
    host: '127.0.0.1', port: PROXY, method: 'POST', path: '/api/token',
    headers: { host: `auth.example.test:${PROXY}`, cookie: `drt_session=${c.value}` },
  }, (res) => {
    let body = '';
    res.on('data', (d) => { body += d; });
    res.on('end', () => (res.statusCode === 200 ? resolve(JSON.parse(body).token) : reject(new Error(`${res.statusCode} ${body}`))));
  });
  req.on('error', reject);
  req.end();
});
const claims = JSON.parse(Buffer.from(token.split('.')[1], 'base64url').toString());
assert.equal(claims.sub, 'alice');
assert.deepEqual(claims.groups, ['admins', 'staff']);
assert.deepEqual(claims.amr, ['pwd', 'otp']);
assert.equal(JSON.parse(Buffer.from(token.split('.')[0], 'base64url').toString()).alg, 'EdDSA');
ok('alice: the login page knows her, and /api/token answers an EdDSA JWT carrying her groups');

// Bob: staff only.
const bob = await browser.newContext();
const { page: bobPage } = await signIn(bob, 'bob', 'bob-pw');
assert.equal(await bobPage.textContent('body'), 'user=bob groups=staff path=/private');
r = await bobPage.goto(`${APP}/admin`);
assert.equal(r.status(), 403);
ok('bob: signed in, and refused /admin, which needs a group he is not in');

// Signing out: the cookie goes, and the app sends Alice to the login again.
await page.goto(`${AUTH}/`);
await page.click('button[type=submit]');
assert.equal((await alice.cookies()).filter((k) => k.name === 'drt_session').length, 0);
await page.goto(`${APP}/private`);
assert.equal(new URL(page.url()).host, `auth.example.test:${PROXY}`);
r = await bobPage.goto(`${APP}/private`);
assert.equal(await bobPage.textContent('body'), 'user=bob groups=staff path=/private');
ok('alice signs out and the app sends her to the login again; bob is still signed in');

// A second sign-in asks for the code, not enrolment.
const again = await alice.newPage();
await again.goto(`${APP}/private`);
await again.fill('#username', 'alice');
await again.fill('#password', 'alice-pw');
await again.click('button[type=submit]');
assert.equal(await again.locator('#totp-secret').count(), 0);
assert.equal(await again.locator('form[action="/2fa/totp"]').count(), 1);
// The next step's code is the way in (the one used at enrolment is spent).
await again.fill('#code', totp(aliceSecret, 1));
await again.click('button[type=submit]');
assert.equal(again.url(), `${APP}/private`);
assert.equal(await again.textContent('body'), 'user=alice groups=admins,staff path=/private');
ok('alice: a second sign-in asks for a code from the app she enrolled, not a new enrolment');

await browser.close();
console.log(`${passed} checks passed`);

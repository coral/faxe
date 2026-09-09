# FAXE website

A static landing page, `/licenses`, and `/privacy`, served by Cloudflare Workers Static
Assets at <https://faxe.oblique.media>. Uses Tailwind CSS, pnpm and Wrangler.
No runtime application server, analytics, external fonts, or client framework.

## Local development

With Node.js 22+ and pnpm 10.28.0, from `web/`:

```sh
pnpm install --frozen-lockfile
pnpm dev
```

Wrangler builds and serves the site locally without requiring a Cloudflare login.
It rebuilds when the watched source files change; refresh to see your edits.
`pnpm check` builds, checks the public file list and links, and validates the
Worker configuration with a deployment dry run. It does not deploy.

The palette in `src/styles.css` matches `src/desktop.rs`. The build copies the
hero directly from `../assets/screenshot_mac.png` and the app icon from
`../packaging/icons/32.png`. Edit the landing page in `src/index.html`.
The privacy policy lives in `src/privacy.html`; keep it aligned with the app's
local storage, network behavior, and the website's hosting whenever those change.

## GitHub deployment setup

1. Create a GitHub environment named **web-production** in this repository.
   Restrict its deployment branches to `master`.
2. Add environment secrets **CLOUDFLARE_API_TOKEN** and
   **CLOUDFLARE_ACCOUNT_ID**. Keep both values out of repository files.
   Use a scoped API token for the intended account and `oblique.media` zone,
   starting from Cloudflare's **Edit Cloudflare Workers** token template.
   The token must support Worker deployment and Custom Domain creation.
3. Push these changes to `master`, or run the **Website** workflow on `master`.
   Wrangler creates/updates `faxe-web` and attaches the Custom Domain in
   `wrangler.jsonc`. The domain must already be an active zone in that account;
   resolve any existing conflicting record for the hostname before deployment.

Pull requests (including forks) build and validate with no deployment secrets.
Deployment runs only for `coral/faxe` on `master`, after checks pass. Actions
are pinned to commit hashes and dependencies use the checked-in pnpm lockfile.
Credentials are passed only to the deployment step, after installation/build.
See Cloudflare's [GitHub Actions guide](https://developers.cloudflare.com/workers/ci-cd/external-cicd/github-actions/)
and [Custom Domains guide](https://developers.cloudflare.com/workers/configuration/routing/custom-domains/).

For a manual deployment, supply the same environment variables through your
shell's secret manager and run `pnpm run deploy` from this directory. Never paste
tokens into commands that will be committed or documented.

For direct Wrangler use, install dependencies once, then deploy from `web/`:

```sh
pnpm install --frozen-lockfile
wrangler deploy
```

Wrangler runs `pnpm build` automatically to create `web/dist` before deployment.
From the repository root, use `wrangler deploy --cwd web`;
bare `wrangler deploy` there cannot find the website configuration.
Add `--dry-run` to validate without publishing.

## Public repository boundary

`wrangler.jsonc` contains the public Worker name and hostname, with no account
ID, zone ID, bindings, credentials, or private environment configuration.
`workers.dev` and preview URLs are disabled. Wrangler's `.wrangler/` state,
`.dev.vars*`, local Wrangler configs, `.env*`, dependencies and build output are
ignored. Keep OAuth state in Wrangler's normal user-level configuration.
Do not commit that directory or copy it into this project.

The build recreates `dist/` from an explicit list of public inputs. It does not
copy whole directories or read environment variables into HTML/CSS. Wrangler
uploads only `dist/`; credentials are never Worker bindings or site assets.
Do not upload Wrangler logs or state as CI artifacts. Git ignores do not protect
files that were already tracked; review staged changes before publishing.

## Acknowledgments

`public/licenses.html` is generated from the reviewed inventory and template in
`../licenses/`. The app's Settings button opens
<https://faxe.oblique.media/licenses>; the app no longer embeds or caches the HTML.
Deploy the website before distributing an app with this link.

Regenerate after dependency changes using the instructions in
[licenses/README.md](../licenses/README.md). From the repository root, the
website-only freshness check is:

```sh
python3 scripts/generate-licenses.py --check-page
```

Native and supplemental source notices remain in `licenses/` for packaging.

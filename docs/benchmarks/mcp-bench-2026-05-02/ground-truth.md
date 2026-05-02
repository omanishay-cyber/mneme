# Ground Truth — MCP Bench 2026-05-02

Built by direct grep over `src/` and `electron/` of internal-app.

## Q1 — Auth functions (gold list)

Files (10):
1. `src/utils/auth.ts` — hashPassword, verifyPassword, generateRecoveryCode, validatePasswordStrength, getLockoutRemaining, createLockoutTimestamp, browserScryptDerive
2. `src/store/useAuthStore.ts` — validateGithubToken, parseSetupToken, login, loginWithSetupToken, logout, restoreSession, localLogin, localLogout, setAuthMode, changePassword, checkOfflineLock, verifyOrgKey, verifyLicense, isSetupComplete
3. `src/utils/permissions.ts` — canAccessPage, canAccessRoute, canViewStore, canEditStore, canManageUsers, isOfflineLocked, getEffectiveRole, hasPermission, canModifyUser
4. `src/hooks/usePermission.ts` — usePermission, usePermissions
5. `src/utils/keyGenerator.ts` — generateStoreKey, validateKeyFormat, maskKey
6. `src/utils/machineId.ts` — getMachineId, clearMachineIdCache
7. `src/utils/orgManager.ts` — checkTechVault, verifyOrgKey, handshakeExistingOrg, createAdminAccount, revokeAdminKey, createManagerOrEmployee, scanAllUsersFromVault
8. `src/utils/techKeyManager.ts` — encryptTechKey, decryptTechKey, validateTechKey
9. `src/pages/LoginPage.tsx` — handleLogin (and form components)
10. `src/components/StartScreen.tsx` — handleLogoClick, handleUserSelect, handlePasswordSubmit, handleStoreSelect, handleTechPortal, getStoresForUser
11. `src/components/ProtectedRoute.tsx` — ProtectedRoute (component)
12. `electron/main.cjs` — IPC handlers: auth:encryptToken, auth:decryptToken, auth:validateGithub, auth:saveAuthStore, auth:loadAuthStore, auth:storePasswordHashes, auth:getPasswordHashes, auth:saveStoreKey, auth:readStoreKey, auth:cacheStoreData, auth:readCachedData, auth:getMachineId, auth:hashPassword, auth:verifyPassword, auth:generateStoreKey, auth:generateRecoveryCode, techkey:encrypt, techkey:decrypt

Total: ~67 distinct named functions across ~12 files.

## Q2 — Blast radius of `src/utils/auth.ts`

Direct importers (grep `from.*utils/auth`):
- src/store/useAuthStore.ts
- src/store/useTechStore.ts (transitive via useAuthStore)
- src/pages/LoginPage.tsx (uses store, indirectly)
- src/components/StartScreen.tsx (uses hashPassword via store)
- src/utils/orgManager.ts (uses hashPassword)
- src/pages/UserProfilePage.tsx (uses changePassword via store)
- electron/main.cjs (re-implements auth:hashPassword IPC)

Indirect (transitive via store): all pages that use `useAuthStore`:
- AdminDashboard, AdminUsersPage, ProtectedRoute, Layout, ActivationWizard, MessagesPage, SettingsPage

Total: ~14 files.

## Q3 — Login call graph

Expected nodes (from LoginPage.tsx outward):
1. LoginPage.tsx:handleLogin -> useAuthStore.login
2. useAuthStore.login -> validateGithubToken (fetches GitHub API)
3. useAuthStore.login -> setAuthMode
4. useAuthStore.login -> persist via electron IPC: auth:saveAuthStore -> safeStorage encrypt
5. LoginPage.tsx -> setup-token path -> parseSetupToken -> loginWithSetupToken -> verifyOrgKey -> orgManager.verifyOrgKey -> GitHub API
6. After login -> ProtectedRoute checks state -> Layout renders

## Q4 — Design patterns

Expected (from code patterns):
1. **Zustand store** (singleton state) — useAuthStore, useAppStore, useTechStore, useThemeStore
2. **Custom hooks** — usePermission(s), useIdleTimer
3. **Higher-order component / wrapper** — ProtectedRoute
4. **IPC bridge / facade** — electron/preload.cjs exposes window.electronAPI
5. **Repository / data access layer** — utils/githubSync.ts, utils/internalFileFormat.ts
6. **Strategy** — different auth modes (none|pat|store_key|admin_key|tech)
7. **Observer / pub-sub** — Zustand subscribers, dataSyncWatcher
8. **Singleton** — store-key cached in machineId.ts, machineId memoized
9. **Factory** — keyGenerator.ts (generateStoreKey, generateRecoveryCode)
10. **Encryption-as-a-service / crypto facade** — techKeyManager (AES-256-GCM)

## Q5 — Security issues in auth

Real issues findable in code:
1. `auth.ts:browserScryptDerive` uses N=16384 (lower than recommended 65536) — weak fallback
2. `useAuthStore.ts:validateGithubToken` stores PAT in localStorage / safeStorage but token visible on object inspection via window.electronAPI debug
3. `electron/main.cjs:auth:cacheStoreData` uses HMAC for tamper-detect but key is derived from store key — single key reuse
4. `permissions.ts:hasPermission` returns `true` for tech role unconditionally — privilege escalation if role check is bypassed
5. `LoginPage.tsx` — no rate limiting on PAT login attempts (lockout exists in auth.ts but not enforced for token login)
6. `auth.ts:generateRecoveryCode` uses `Math.random()` — not cryptographically secure (CHECK: does code use crypto.randomBytes via IPC?)
7. `machineId.ts` — uses WMIC subprocess (unsanitized command injection if env var manipulation possible)
8. `useAuthStore.ts:setupTokenStr` parsing — no length limit on raw input could enable DoS or buffer issue

-- When the current run of failed sign-ins started. Failures count towards the lockout
-- threshold only while they fall within `auth.lockout.consecutivePeriod` of this time.
ALTER TABLE users ADD COLUMN failed_logins_since timestamptz;
UPDATE users SET failed_logins_since = now() WHERE failed_logins > 0;

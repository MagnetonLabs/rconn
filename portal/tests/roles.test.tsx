import { describe, expect, it } from 'vitest';
import { renderToString } from 'react-dom/server';
import { MemoryRouter } from 'react-router';
import { assignableRoles, can, canManageMember } from '../src/lib/roles';
import { OrgCtx } from '../src/lib/org';
import { ComputersTabs } from '../src/pages/Computers';
import type { Role } from '../src/api/auth';

describe('role table', () => {
  const table: [Role, Record<string, boolean>][] = [
    ['owner', { 'computers.manage': true, 'people.manage': true, 'audit.view': true, 'sso.manage': true }],
    ['admin', { 'computers.manage': true, 'people.manage': true, 'audit.view': true, 'sso.manage': false }],
    ['technician', { 'computers.manage': false, 'people.manage': false, 'audit.view': false, 'sso.manage': false }],
    ['viewer', { 'computers.manage': false, 'people.manage': false, 'audit.view': false, 'sso.manage': false }],
  ];
  it.each(table)('%s', (role, expected) => {
    for (const [action, allowed] of Object.entries(expected)) {
      expect(can(role, action as Parameters<typeof can>[1]), `${role} ${action}`).toBe(allowed);
    }
  });

  it('admins cannot hand out or touch owner', () => {
    expect(assignableRoles('admin')).not.toContain('owner');
    expect(assignableRoles('owner')).toContain('owner');
    expect(assignableRoles('technician')).toEqual([]);
    expect(canManageMember('admin', 'owner', { self: false, ownerCount: 2 })).toBe(false);
    expect(canManageMember('admin', 'technician', { self: false, ownerCount: 1 })).toBe(true);
    expect(canManageMember('owner', 'owner', { self: false, ownerCount: 2 })).toBe(true);
    expect(canManageMember('owner', 'owner', { self: false, ownerCount: 1 })).toBe(false);
    expect(canManageMember('owner', 'admin', { self: true, ownerCount: 2 })).toBe(false);
    expect(canManageMember('viewer', 'viewer', { self: false, ownerCount: 1 })).toBe(false);
  });
});

describe('gated UI', () => {
  const render = (role: Role) =>
    renderToString(
      <MemoryRouter>
        <OrgCtx.Provider value={{ org_id: 'o1', org_name: 'Acme', role }}>
          <ComputersTabs />
        </OrgCtx.Provider>
      </MemoryRouter>,
    );
  it('hides the enrollment tokens tab from viewers and technicians', () => {
    expect(render('viewer')).not.toContain('Enrollment tokens');
    expect(render('technician')).not.toContain('Enrollment tokens');
    expect(render('admin')).toContain('Enrollment tokens');
  });
});

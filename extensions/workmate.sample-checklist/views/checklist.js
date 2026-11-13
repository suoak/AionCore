/* Plan checklist — a small client-side sample for CSBU WorkMate.
 *
 * State lives in memory only. This file deliberately uses no storage, no network and no
 * dynamic code evaluation, and it never builds HTML from user text (everything is written
 * with textContent and attributes). */
(function () {
  'use strict';

  var MAX_ITEMS = 100;
  var MAX_LENGTH = 200;

  var MESSAGES = {
    en: {
      title: 'Plan checklist',
      subtitle: 'Break the work into steps and tick them off as you go.',
      progress: 'Progress',
      addLabel: 'New step',
      addPlaceholder: 'Add a step and press Enter',
      add: 'Add',
      emptyTitle: 'No steps yet',
      emptyText: 'Write your own, or start from a simple plan and edit it.',
      starter: 'Add a starter plan',
      listLabel: 'Steps',
      filterLabel: 'Show',
      filterAll: 'All',
      filterOpen: 'To do',
      filterDone: 'Done',
      clearDone: 'Clear done',
      memoryNote: 'Steps live only in this panel and are cleared when it reloads. Nothing is saved or sent anywhere.',
      nothingHere: 'Nothing in this view.',
      edit: 'Edit',
      remove: 'Delete',
      moveUp: 'Move up',
      moveDown: 'Move down',
      save: 'Save',
      cancel: 'Cancel',
      editLabel: 'Edit step',
      limit: 'The list is full (100 steps).',
      added: 'Step added.',
      removed: 'Step deleted.',
      cleared: 'Cleared completed steps.',
      allDone: 'All steps are done.',
      starterPlan: [
        'Clarify the goal and what “done” looks like',
        'Gather the files and context you need',
        'Draft the first version',
        'Review, test and fix the rough edges',
        'Share the result and note follow-ups',
      ],
    },
    'zh-CN': {
      title: '计划清单',
      subtitle: '把工作拆成小步骤，完成一项就勾掉一项。',
      progress: '进度',
      addLabel: '新步骤',
      addPlaceholder: '输入步骤，按回车添加',
      add: '添加',
      emptyTitle: '还没有步骤',
      emptyText: '可以自己写，也可以从一个简单的计划开始再修改。',
      starter: '添加示例计划',
      listLabel: '步骤',
      filterLabel: '显示',
      filterAll: '全部',
      filterOpen: '待办',
      filterDone: '已完成',
      clearDone: '清除已完成',
      memoryNote: '步骤只保存在此面板中，刷新后会清空。不会保存或发送到任何地方。',
      nothingHere: '此视图中没有内容。',
      edit: '编辑',
      remove: '删除',
      moveUp: '上移',
      moveDown: '下移',
      save: '保存',
      cancel: '取消',
      editLabel: '编辑步骤',
      limit: '清单已满（最多 100 项）。',
      added: '已添加步骤。',
      removed: '已删除步骤。',
      cleared: '已清除已完成的步骤。',
      allDone: '所有步骤都已完成。',
      starterPlan: [
        '明确目标，以及“完成”的标准',
        '收集需要的文件和背景信息',
        '起草第一版',
        '检查、测试并修补细节',
        '分享成果并记录后续事项',
      ],
    },
  };

  function pickLocale() {
    var langs = (navigator.languages && navigator.languages.length ? navigator.languages : [navigator.language]) || [];
    for (var i = 0; i < langs.length; i += 1) {
      if (/^zh\b/i.test(String(langs[i]))) return 'zh-CN';
      if (/^en\b/i.test(String(langs[i]))) return 'en';
    }
    return 'en';
  }

  var locale = pickLocale();
  var t = MESSAGES[locale];
  document.documentElement.lang = locale;

  var state = { items: [], filter: 'all', editing: null, nextId: 1 };

  var els = {
    add: document.getElementById('add-button'),
    input: document.getElementById('add-input'),
    list: document.getElementById('list'),
    empty: document.getElementById('empty'),
    starter: document.getElementById('starter'),
    toolbar: document.getElementById('toolbar'),
    clearDone: document.getElementById('clear-done'),
    count: document.getElementById('progress-count'),
    bar: document.getElementById('progress-bar'),
    fill: document.getElementById('progress-fill'),
    status: document.getElementById('status'),
  };

  function applyStaticText() {
    var nodes = document.querySelectorAll('[data-i18n]');
    for (var i = 0; i < nodes.length; i += 1) {
      var key = nodes[i].getAttribute('data-i18n');
      if (key && typeof t[key] === 'string') nodes[i].textContent = t[key];
    }
    var placeholders = document.querySelectorAll('[data-i18n-placeholder]');
    for (var j = 0; j < placeholders.length; j += 1) {
      var pk = placeholders[j].getAttribute('data-i18n-placeholder');
      if (pk && typeof t[pk] === 'string') placeholders[j].setAttribute('placeholder', t[pk]);
    }
    var labels = document.querySelectorAll('[data-i18n-aria-label]');
    for (var k = 0; k < labels.length; k += 1) {
      var lk = labels[k].getAttribute('data-i18n-aria-label');
      if (lk && typeof t[lk] === 'string') labels[k].setAttribute('aria-label', t[lk]);
    }
    document.title = t.title;
  }

  function announce(message) {
    els.status.textContent = '';
    els.status.textContent = message;
  }

  function counts() {
    var done = 0;
    for (var i = 0; i < state.items.length; i += 1) if (state.items[i].done) done += 1;
    return { done: done, total: state.items.length };
  }

  function visibleItems() {
    return state.items.filter(function (item) {
      if (state.filter === 'open') return !item.done;
      if (state.filter === 'done') return item.done;
      return true;
    });
  }

  function indexOfId(id) {
    for (var i = 0; i < state.items.length; i += 1) if (state.items[i].id === id) return i;
    return -1;
  }

  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  }

  function iconButton(action, label, glyph, disabled) {
    var button = el('button', 'icon-btn', glyph);
    button.type = 'button';
    button.setAttribute('data-action', action);
    button.setAttribute('aria-label', label);
    button.title = label;
    if (disabled) button.disabled = true;
    return button;
  }

  function textButton(action, label, className) {
    var button = el('button', className, label);
    button.type = 'button';
    button.setAttribute('data-action', action);
    return button;
  }

  function renderItem(item, position, lastPosition, canReorder) {
    var li = el('li', 'item' + (item.done ? ' is-done' : ''));
    li.setAttribute('data-id', String(item.id));

    var check = document.createElement('input');
    check.type = 'checkbox';
    check.className = 'check';
    check.checked = item.done;
    check.id = 'step-' + item.id;
    check.setAttribute('data-action', 'toggle');
    li.appendChild(check);

    if (state.editing === item.id) {
      var editor = document.createElement('input');
      editor.type = 'text';
      editor.className = 'edit-input';
      editor.maxLength = MAX_LENGTH;
      editor.value = item.text;
      editor.setAttribute('aria-label', t.editLabel);
      editor.setAttribute('data-role', 'editor');
      li.appendChild(editor);
      var actions = el('div', 'actions always');
      actions.appendChild(textButton('save', t.save, 'btn small primary'));
      actions.appendChild(textButton('cancel', t.cancel, 'btn small'));
      li.appendChild(actions);
      return li;
    }

    var label = el('label', 'text', item.text);
    label.setAttribute('for', check.id);
    li.appendChild(label);

    var tools = el('div', 'actions');
    if (canReorder) {
      tools.appendChild(iconButton('up', t.moveUp, '↑', position === 0));
      tools.appendChild(iconButton('down', t.moveDown, '↓', position === lastPosition));
    }
    tools.appendChild(iconButton('edit', t.edit, '✎', false));
    tools.appendChild(iconButton('remove', t.remove, '×', false));
    li.appendChild(tools);
    return li;
  }

  function render() {
    var c = counts();
    var percent = c.total === 0 ? 0 : Math.round((c.done / c.total) * 100);
    els.count.textContent = c.done + ' / ' + c.total;
    els.bar.setAttribute('aria-valuenow', String(percent));
    els.bar.setAttribute('aria-valuetext', c.done + ' / ' + c.total);
    els.fill.style.width = percent + '%';
    els.fill.classList.toggle('complete', c.total > 0 && c.done === c.total);

    var hasItems = c.total > 0;
    els.empty.hidden = hasItems;
    els.toolbar.hidden = !hasItems;
    els.clearDone.disabled = c.done === 0;

    var chips = document.querySelectorAll('[data-filter]');
    for (var i = 0; i < chips.length; i += 1) {
      chips[i].setAttribute('aria-pressed', String(chips[i].getAttribute('data-filter') === state.filter));
    }

    var visible = visibleItems();
    var canReorder = state.filter === 'all';
    while (els.list.firstChild) els.list.removeChild(els.list.firstChild);
    for (var p = 0; p < visible.length; p += 1) {
      els.list.appendChild(renderItem(visible[p], p, visible.length - 1, canReorder));
    }
    if (hasItems && visible.length === 0) {
      var none = el('li', 'none', t.nothingHere);
      els.list.appendChild(none);
    }

    if (state.editing !== null) {
      var editor = els.list.querySelector('[data-role="editor"]');
      if (editor) {
        editor.focus();
        editor.setSelectionRange(editor.value.length, editor.value.length);
      }
    }
  }

  function clean(value) {
    return String(value).replace(/\s+/g, ' ').trim().slice(0, MAX_LENGTH);
  }

  function addItem(text) {
    var value = clean(text);
    if (!value) return false;
    if (state.items.length >= MAX_ITEMS) {
      announce(t.limit);
      return false;
    }
    state.items.push({ id: state.nextId, text: value, done: false });
    state.nextId += 1;
    return true;
  }

  // No <form>: the panel sandbox does not grant allow-forms, so submit would be blocked.
  function submitNewStep() {
    if (addItem(els.input.value)) {
      els.input.value = '';
      render();
      announce(t.added);
    }
    els.input.focus();
  }

  els.add.addEventListener('click', submitNewStep);
  els.input.addEventListener('keydown', function (event) {
    if (event.key === 'Enter' && !event.isComposing) {
      event.preventDefault();
      submitNewStep();
    }
  });

  els.starter.addEventListener('click', function () {
    for (var i = 0; i < t.starterPlan.length; i += 1) addItem(t.starterPlan[i]);
    state.filter = 'all';
    render();
    els.input.focus();
  });

  els.clearDone.addEventListener('click', function () {
    state.items = state.items.filter(function (item) {
      return !item.done;
    });
    if (state.editing !== null && indexOfId(state.editing) === -1) state.editing = null;
    if (state.filter === 'done') state.filter = 'all';
    render();
    announce(t.cleared);
    els.input.focus();
  });

  document.querySelector('.filters').addEventListener('click', function (event) {
    var chip = event.target.closest('[data-filter]');
    if (!chip) return;
    state.filter = chip.getAttribute('data-filter');
    state.editing = null;
    render();
  });

  function itemIdFrom(node) {
    var li = node.closest('li[data-id]');
    return li ? Number(li.getAttribute('data-id')) : null;
  }

  function commitEdit(id, value) {
    var index = indexOfId(id);
    var next = clean(value);
    if (index !== -1 && next) state.items[index].text = next;
    state.editing = null;
    render();
    var back = els.list.querySelector('li[data-id="' + id + '"] [data-action="edit"]');
    if (back) back.focus();
  }

  els.list.addEventListener('change', function (event) {
    var target = event.target;
    if (!target || target.getAttribute('data-action') !== 'toggle') return;
    var id = itemIdFrom(target);
    var index = id === null ? -1 : indexOfId(id);
    if (index === -1) return;
    state.items[index].done = target.checked;
    var c = counts();
    render();
    if (c.total > 0 && c.done === c.total) announce(t.allDone);
    var again = els.list.querySelector('li[data-id="' + id + '"] .check');
    if (again) again.focus();
  });

  els.list.addEventListener('click', function (event) {
    var button = event.target.closest('button[data-action]');
    if (!button) return;
    var id = itemIdFrom(button);
    var index = id === null ? -1 : indexOfId(id);
    if (index === -1) return;
    var action = button.getAttribute('data-action');

    if (action === 'remove') {
      state.items.splice(index, 1);
      if (state.editing === id) state.editing = null;
      render();
      announce(t.removed);
      els.input.focus();
    } else if (action === 'up' && index > 0) {
      var up = state.items[index - 1];
      state.items[index - 1] = state.items[index];
      state.items[index] = up;
      render();
      focusAction(id, 'up', 'down');
    } else if (action === 'down' && index < state.items.length - 1) {
      var down = state.items[index + 1];
      state.items[index + 1] = state.items[index];
      state.items[index] = down;
      render();
      focusAction(id, 'down', 'up');
    } else if (action === 'edit') {
      state.editing = id;
      render();
    } else if (action === 'save') {
      var editor = button.closest('li').querySelector('[data-role="editor"]');
      commitEdit(id, editor ? editor.value : '');
    } else if (action === 'cancel') {
      state.editing = null;
      render();
      focusAction(id, 'edit', 'edit');
    }
  });

  function focusAction(id, preferred, fallback) {
    var row = els.list.querySelector('li[data-id="' + id + '"]');
    if (!row) return;
    var target = row.querySelector('[data-action="' + preferred + '"]:not(:disabled)') ||
      row.querySelector('[data-action="' + fallback + '"]:not(:disabled)');
    if (target) target.focus();
  }

  els.list.addEventListener('keydown', function (event) {
    var target = event.target;
    if (!target || target.getAttribute('data-role') !== 'editor') return;
    var id = itemIdFrom(target);
    if (id === null) return;
    if (event.key === 'Enter') {
      event.preventDefault();
      commitEdit(id, target.value);
    } else if (event.key === 'Escape') {
      event.preventDefault();
      state.editing = null;
      render();
      focusAction(id, 'edit', 'edit');
    }
  });

  applyStaticText();
  render();
})();

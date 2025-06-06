import 'package:appflowy/plugins/document/application/document_bloc.dart';
import 'package:appflowy/plugins/document/presentation/editor_plugins/ai/operations/ai_writer_node_extension.dart';
import 'package:appflowy/plugins/document/presentation/editor_plugins/plugins.dart';
import 'package:appflowy_backend/protobuf/flowy-folder/protobuf.dart';
import 'package:appflowy_editor/appflowy_editor.dart';
import 'package:universal_platform/universal_platform.dart';

import 'slash_menu_items/mobile_items.dart';
import 'slash_menu_items/slash_menu_items.dart';

/// Build slash menu items
///
List<SelectionMenuItem> slashMenuItemsBuilder({
  bool isLocalMode = false,
  DocumentBloc? documentBloc,
  EditorState? editorState,
  Node? node,
  ViewPB? view,
}) {
  final isInTable = node != null && node.parentTableCellNode != null;
  final isMobile = UniversalPlatform.isMobile;
  bool isEmpty = false;
  if (editorState == null || editorState.isEmptyForContinueWriting()) {
    if (view == null || view.name.isEmpty) {
      isEmpty = true;
    }
  }
  if (isMobile) {
    if (isInTable) {
      return mobileItemsInTale;
    } else {
      return mobileItems;
    }
  } else {
    if (isInTable) {
      return _simpleTableSlashMenuItems();
    } else {
      return _defaultSlashMenuItems(
        isLocalMode: isLocalMode,
        documentBloc: documentBloc,
        isEmpty: isEmpty,
      );
    }
  }
}

/// The default slash menu items are used in the text-based block.
///
/// Except for the simple table block, the slash menu items in the table block are
/// built by the `tableSlashMenuItem` function.
/// If in local mode, disable the ai writer feature
///
/// The linked database relies on the documentBloc, so it's required to pass in
/// the documentBloc when building the slash menu items. If the documentBloc is
/// not provided, the linked database items will be disabled.
///
///
List<SelectionMenuItem> _defaultSlashMenuItems({
  bool isLocalMode = false,
  DocumentBloc? documentBloc,
  bool isEmpty = false,
}) {
  return [
    // ai
    if (!isEmpty) continueWritingSlashMenuItem,
    aiWriterSlashMenuItem,

    paragraphSlashMenuItem,

    // heading 1-3
    heading1SlashMenuItem,
    heading2SlashMenuItem,
    heading3SlashMenuItem,

    // image
    imageSlashMenuItem,

    // list
    bulletedListSlashMenuItem,
    numberedListSlashMenuItem,
    todoListSlashMenuItem,

    // divider
    dividerSlashMenuItem,

    // quote
    quoteSlashMenuItem,

    // simple table
    tableSlashMenuItem,

    // link to page
    linkToPageSlashMenuItem,

    // columns
    // 2-4 columns
    twoColumnsSlashMenuItem,
    threeColumnsSlashMenuItem,
    fourColumnsSlashMenuItem,

    // grid
    if (documentBloc != null) gridSlashMenuItem(documentBloc),
    referencedGridSlashMenuItem,

    // kanban
    if (documentBloc != null) kanbanSlashMenuItem(documentBloc),
    referencedKanbanSlashMenuItem,

    // calendar
    if (documentBloc != null) calendarSlashMenuItem(documentBloc),
    referencedCalendarSlashMenuItem,

    // callout
    calloutSlashMenuItem,

    // outline
    outlineSlashMenuItem,

    // math equation
    mathEquationSlashMenuItem,

    // code block
    codeBlockSlashMenuItem,

    // toggle list - toggle headings
    toggleListSlashMenuItem,
    toggleHeading1SlashMenuItem,
    toggleHeading2SlashMenuItem,
    toggleHeading3SlashMenuItem,

    // emoji
    emojiSlashMenuItem,

    // date or reminder
    dateOrReminderSlashMenuItem,

    // photo gallery
    photoGallerySlashMenuItem,

    // file
    fileSlashMenuItem,

    // sub page
    subPageSlashMenuItem,
  ];
}

/// The slash menu items in the simple table block.
///
/// There're some blocks should be excluded in the slash menu items.
///
/// - Database Items
/// - Image Gallery
List<SelectionMenuItem> _simpleTableSlashMenuItems() {
  return [
    paragraphSlashMenuItem,

    // heading 1-3
    heading1SlashMenuItem,
    heading2SlashMenuItem,
    heading3SlashMenuItem,

    // image
    imageSlashMenuItem,

    // list
    bulletedListSlashMenuItem,
    numberedListSlashMenuItem,
    todoListSlashMenuItem,

    // divider
    dividerSlashMenuItem,

    // quote
    quoteSlashMenuItem,

    // link to page
    linkToPageSlashMenuItem,

    // callout
    calloutSlashMenuItem,

    // math equation
    mathEquationSlashMenuItem,

    // code block
    codeBlockSlashMenuItem,

    // toggle list - toggle headings
    toggleListSlashMenuItem,
    toggleHeading1SlashMenuItem,
    toggleHeading2SlashMenuItem,
    toggleHeading3SlashMenuItem,

    // emoji
    emojiSlashMenuItem,

    // date or reminder
    dateOrReminderSlashMenuItem,

    // file
    fileSlashMenuItem,

    // sub page
    subPageSlashMenuItem,
  ];
}

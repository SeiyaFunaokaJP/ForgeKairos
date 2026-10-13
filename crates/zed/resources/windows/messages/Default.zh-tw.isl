; *** Inno Setup version 6.5.0+ Chinese Traditional (Taiwan) messages ***
;
; Translated for ForgeKairos from Inno Setup's English Default.isl, with the
; Simplified Chinese file next to this one as the structural model. Wording
; follows Microsoft's Taiwan (zh-TW) terminology.
;
; Note: When translating this text, do not add periods (.) to the end of
; messages that didn't have them already, because on those messages Inno
; Setup adds the periods automatically (appending a period would result in
; two periods being displayed).

[LangOptions]
; The following three entries are very important. Be sure to read and
; understand the '[LangOptions] section' topic in the help file.
LanguageName=繁體中文
; About LanguageID, to reference link:
; https://docs.microsoft.com/en-us/openspecs/windows_protocols/ms-lcid/a9eac961-e77d-41a6-90a5-ce1a8b0cdb9c
LanguageID=$0404
; About CodePage, to reference link:
; https://docs.microsoft.com/en-us/windows/win32/intl/code-page-identifiers
LanguageCodePage=950
; If the language you are translating to requires special font faces or
; sizes, uncomment any of the following entries and change them accordingly.
;DialogFontName=
;DialogFontSize=9
;DialogFontBaseScaleWidth=7
;DialogFontBaseScaleHeight=15
;WelcomeFontName=Segoe UI
;WelcomeFontSize=14

[Messages]

; *** Application titles
SetupAppTitle=安裝程式
SetupWindowTitle=安裝程式 - %1
UninstallAppTitle=解除安裝
UninstallAppFullTitle=解除安裝 %1

; *** Misc. common
InformationTitle=資訊
ConfirmTitle=確認
ErrorTitle=錯誤

; *** SetupLdr messages
SetupLdrStartupMessage=即將安裝 %1。您要繼續嗎？
LdrCannotCreateTemp=無法建立暫存檔案。安裝程式已中止
LdrCannotExecTemp=無法執行暫存資料夾中的檔案。安裝程式已中止
HelpTextNote=

; *** Startup error messages
LastErrorMessage=%1。%n%n錯誤 %2：%3
SetupFileMissing=安裝資料夾中缺少檔案 %1。請修正這個問題，或取得新的程式副本。
SetupFileCorrupt=安裝檔案已損毀。請取得新的程式副本。
SetupFileCorruptOrWrongVer=安裝檔案已損毀，或與此版本的安裝程式不相容。請修正這個問題，或取得新的程式副本。
InvalidParameter=命令列傳入了無效的參數：%n%n%1
SetupAlreadyRunning=安裝程式已在執行中。
WindowsVersionNotSupported=此程式不支援您電腦上執行的 Windows 版本。
WindowsServicePackRequired=此程式需要 %1 Service Pack %2 或更新版本。
NotOnThisPlatform=此程式無法在 %1 上執行。
OnlyOnThisPlatform=此程式必須在 %1 上執行。
OnlyOnTheseArchitectures=此程式只能安裝在為下列處理器架構設計的 Windows 版本上：%n%n%1
WinVersionTooLowError=此程式需要 %1 版本 %2 或更新版本。
WinVersionTooHighError=此程式無法安裝在 %1 版本 %2 或更新版本上。
AdminPrivilegesRequired=安裝此程式時，您必須以系統管理員身分登入。
PowerUserPrivilegesRequired=安裝此程式時，您必須以系統管理員或 Power Users 群組成員的身分登入。
SetupAppRunningError=安裝程式偵測到 %1 正在執行。%n%n請立即關閉它的所有執行個體，然後按一下「確定」繼續，或按一下「取消」結束。
UninstallAppRunningError=解除安裝程式偵測到 %1 正在執行。%n%n請立即關閉它的所有執行個體，然後按一下「確定」繼續，或按一下「取消」結束。

; *** Startup questions
PrivilegesRequiredOverrideTitle=選取安裝程式的安裝模式
PrivilegesRequiredOverrideInstruction=選取安裝模式
PrivilegesRequiredOverrideText1=%1 可以為所有使用者安裝 (需要系統管理員權限)，或只為您安裝。
PrivilegesRequiredOverrideText2=%1 可以只為您安裝，或為所有使用者安裝 (需要系統管理員權限)。
PrivilegesRequiredOverrideAllUsers=為所有使用者安裝(&A)
PrivilegesRequiredOverrideAllUsersRecommended=為所有使用者安裝(&A) (建議)
PrivilegesRequiredOverrideCurrentUser=只為我安裝(&M)
PrivilegesRequiredOverrideCurrentUserRecommended=只為我安裝(&M) (建議)

; *** Misc. errors
ErrorCreatingDir=安裝程式無法建立資料夾「%1」
ErrorTooManyFilesInDir=無法在資料夾「%1」中建立檔案，因為其中的檔案太多

; *** Setup common messages
ExitSetupTitle=結束安裝程式
ExitSetupMessage=安裝尚未完成。如果現在結束，將不會安裝此程式。%n%n您可以稍後再次執行安裝程式來完成安裝。%n%n要結束安裝程式嗎？
AboutSetupMenuItem=關於安裝程式(&A)...
AboutSetupTitle=關於安裝程式
AboutSetupMessage=%1 版本 %2%n%3%n%n%1 首頁：%n%4
AboutSetupNote=
TranslatorNote=

; *** Buttons
ButtonBack=< 上一步(&B)
ButtonNext=下一步(&N) >
ButtonInstall=安裝(&I)
ButtonOK=確定
ButtonCancel=取消
ButtonYes=是(&Y)
ButtonYesToAll=全部皆是(&A)
ButtonNo=否(&N)
ButtonNoToAll=全部皆否(&O)
ButtonFinish=完成(&F)
ButtonBrowse=瀏覽(&B)...
ButtonWizardBrowse=瀏覽(&R)...
ButtonNewFolder=新增資料夾(&M)

; *** "Select Language" dialog messages
SelectLanguageTitle=選取安裝語言
SelectLanguageLabel=選取安裝期間要使用的語言。

; *** Common wizard text
ClickNext=按一下「下一步」繼續，或按一下「取消」結束安裝程式。
BeveledLabel=
BrowseDialogTitle=瀏覽資料夾
BrowseDialogLabel=在下列清單中選取資料夾，然後按一下「確定」。
NewFolderName=新增資料夾

; *** "Welcome" wizard page
WelcomeLabel1=歡迎使用 [name] 安裝精靈
WelcomeLabel2=即將在您的電腦上安裝 [name/ver]。%n%n建議您先關閉其他所有應用程式再繼續。

; *** "Password" wizard page
WizardPassword=密碼
PasswordLabel1=此安裝受密碼保護。
PasswordLabel3=請輸入密碼，然後按一下「下一步」繼續。密碼須區分大小寫。
PasswordEditLabel=密碼(&P)：
IncorrectPassword=您輸入的密碼不正確。請再試一次。

; *** "License Agreement" wizard page
WizardLicense=授權合約
LicenseLabel=請先閱讀下列重要資訊再繼續。
LicenseLabel3=請閱讀下列授權合約。您必須接受此合約的條款，才能繼續安裝。
LicenseAccepted=我接受合約(&A)
LicenseNotAccepted=我不接受合約(&D)

; *** "Information" wizard pages
WizardInfoBefore=資訊
InfoBeforeLabel=請先閱讀下列重要資訊再繼續。
InfoBeforeClickLabel=準備好繼續安裝時，請按一下「下一步」。
WizardInfoAfter=資訊
InfoAfterLabel=請先閱讀下列重要資訊再繼續。
InfoAfterClickLabel=準備好繼續安裝時，請按一下「下一步」。

; *** "User Information" wizard page
WizardUserInfo=使用者資訊
UserInfoDesc=請輸入您的資訊。
UserInfoName=使用者名稱(&U)：
UserInfoOrg=組織(&O)：
UserInfoSerial=序號(&S)：
UserInfoNameRequired=您必須輸入名稱。

; *** "Select Destination Location" wizard page
WizardSelectDir=選取目的地位置
SelectDirDesc=要將 [name] 安裝在哪裡？
SelectDirLabel3=安裝程式會將 [name] 安裝到下列資料夾。
SelectDirBrowseLabel=按一下「下一步」繼續。如果要選取其他資料夾，請按一下「瀏覽」。
DiskSpaceGBLabel=至少需要 [gb] GB 的可用磁碟空間。
DiskSpaceMBLabel=至少需要 [mb] MB 的可用磁碟空間。
CannotInstallToNetworkDrive=安裝程式無法安裝到網路磁碟機。
CannotInstallToUNCPath=安裝程式無法安裝到 UNC 路徑。
InvalidPath=您必須輸入含磁碟機代號的完整路徑，例如：%n%nC:\APP%n%n或下列格式的 UNC 路徑：%n%n\\server\share
InvalidDrive=您選取的磁碟機或 UNC 共用不存在或無法存取。請選取其他位置。
DiskSpaceWarningTitle=磁碟空間不足
DiskSpaceWarning=安裝程式至少需要 %1 KB 的可用空間才能安裝，但選取的磁碟機只有 %2 KB 可用。%n%n仍要繼續嗎？
DirNameTooLong=資料夾名稱或路徑太長。
InvalidDirName=資料夾名稱無效。
BadDirName32=資料夾名稱不能包含下列任何字元：%n%n%1
DirExistsTitle=資料夾已存在
DirExists=資料夾：%n%n%1%n%n已經存在。仍要安裝到該資料夾嗎？
DirDoesntExistTitle=資料夾不存在
DirDoesntExist=資料夾：%n%n%1%n%n不存在。要建立此資料夾嗎？

; *** "Select Components" wizard page
WizardSelectComponents=選取元件
SelectComponentsDesc=要安裝哪些元件？
SelectComponentsLabel2=選取您要安裝的元件，並清除您不要安裝的元件。準備好繼續時，請按一下「下一步」。
FullInstallation=完整安裝
; if possible don't translate 'Compact' as 'Minimal' (I mean 'Minimal' in your language)
CompactInstallation=精簡安裝
CustomInstallation=自訂安裝
NoUninstallWarningTitle=元件已存在
NoUninstallWarning=安裝程式偵測到您的電腦上已安裝下列元件：%n%n%1%n%n取消選取這些元件並不會將其解除安裝。%n%n仍要繼續嗎？
ComponentSize1=%1 KB
ComponentSize2=%1 MB
ComponentsDiskSpaceGBLabel=目前的選取項目至少需要 [gb] GB 的磁碟空間。
ComponentsDiskSpaceMBLabel=目前的選取項目至少需要 [mb] MB 的磁碟空間。

; *** "Select Additional Tasks" wizard page
WizardSelectTasks=選取其他工作
SelectTasksDesc=要執行哪些其他工作？
SelectTasksLabel2=選取您要安裝程式在安裝 [name] 時執行的其他工作，然後按一下「下一步」。

; *** "Select Start Menu Folder" wizard page
WizardSelectProgramGroup=選取「開始」功能表資料夾
SelectStartMenuFolderDesc=安裝程式應將程式的捷徑放在哪裡？
SelectStartMenuFolderLabel3=安裝程式會在下列「開始」功能表資料夾中建立程式的捷徑。
SelectStartMenuFolderBrowseLabel=按一下「下一步」繼續。如果要選取其他資料夾，請按一下「瀏覽」。
MustEnterGroupName=您必須輸入資料夾名稱。
GroupNameTooLong=資料夾名稱或路徑太長。
InvalidGroupName=資料夾名稱無效。
BadGroupName=資料夾名稱不能包含下列任何字元：%n%n%1
NoProgramGroupCheck2=不要建立「開始」功能表資料夾(&D)

; *** "Ready to Install" wizard page
WizardReady=準備安裝
ReadyLabel1=安裝程式已準備好開始在您的電腦上安裝 [name]。
ReadyLabel2a=按一下「安裝」繼續安裝；如果要檢閱或變更任何設定，請按一下「上一步」。
ReadyLabel2b=按一下「安裝」繼續安裝。
ReadyMemoUserInfo=使用者資訊：
ReadyMemoDir=目的地位置：
ReadyMemoType=安裝類型：
ReadyMemoComponents=選取的元件：
ReadyMemoGroup=「開始」功能表資料夾：
ReadyMemoTasks=其他工作：

; *** TDownloadWizardPage wizard page and DownloadTemporaryFile
DownloadingLabel2=正在下載檔案...
ButtonStopDownload=停止下載(&S)
StopDownload=確定要停止下載嗎？
ErrorDownloadAborted=下載已中止
ErrorDownloadFailed=下載失敗：%1 %2
ErrorDownloadSizeFailed=取得大小失敗：%1 %2
ErrorProgress=無效的進度：%1 / %2
ErrorFileSize=無效的檔案大小：預期為 %1，實際為 %2

; *** TExtractionWizardPage wizard page and ExtractArchive
ExtractingLabel=正在解壓縮檔案...
ButtonStopExtraction=停止解壓縮(&S)
StopExtraction=確定要停止解壓縮嗎？
ErrorExtractionAborted=解壓縮已中止
ErrorExtractionFailed=解壓縮失敗：%1

; *** Archive extraction failure details
ArchiveIncorrectPassword=密碼不正確
ArchiveIsCorrupted=封存檔已損毀
ArchiveUnsupportedFormat=不支援此封存檔格式

; *** "Preparing to Install" wizard page
WizardPreparing=正在準備安裝
PreparingDesc=安裝程式正在準備在您的電腦上安裝 [name]。
PreviousInstallNotCompleted=先前程式的安裝或移除尚未完成。您必須重新啟動電腦才能完成該安裝。%n%n重新啟動電腦後，請再次執行安裝程式以完成 [name] 的安裝。
CannotContinue=安裝程式無法繼續。請按一下「取消」結束。
ApplicationsFound=下列應用程式正在使用安裝程式需要更新的檔案。建議您允許安裝程式自動關閉這些應用程式。
ApplicationsFound2=下列應用程式正在使用安裝程式需要更新的檔案。建議您允許安裝程式自動關閉這些應用程式。安裝完成後，安裝程式會嘗試重新啟動這些應用程式。
CloseApplications=自動關閉應用程式(&A)
DontCloseApplications=不要關閉應用程式(&D)
ErrorCloseApplications=安裝程式無法自動關閉所有應用程式。建議您先關閉所有正在使用安裝程式需要更新之檔案的應用程式，再繼續。
PrepareToInstallNeedsRestart=安裝程式必須重新啟動您的電腦。重新啟動電腦後，請再次執行安裝程式以完成 [name] 的安裝。%n%n要立即重新啟動嗎？

; *** "Installing" wizard page
WizardInstalling=正在安裝
InstallingLabel=安裝程式正在您的電腦上安裝 [name]，請稍候。

; *** "Setup Completed" wizard page
FinishedHeadingLabel=正在完成 [name] 安裝精靈
FinishedLabelNoIcons=安裝程式已在您的電腦上完成 [name] 的安裝。
FinishedLabel=安裝程式已在您的電腦上完成 [name] 的安裝。您可以選取已安裝的捷徑來啟動此應用程式。
ClickFinish=按一下「完成」結束安裝程式。
FinishedRestartLabel=安裝程式必須重新啟動您的電腦，才能完成 [name] 的安裝。要立即重新啟動嗎？
FinishedRestartMessage=安裝程式必須重新啟動您的電腦，才能完成 [name] 的安裝。%n%n要立即重新啟動嗎？
ShowReadmeCheck=是，我要檢視讀我檔案
YesRadio=是，立即重新啟動電腦(&Y)
NoRadio=否，我稍後再重新啟動電腦(&N)
; used for example as 'Run MyProg.exe'
RunEntryExec=執行 %1
; used for example as 'View Readme.txt'
RunEntryShellExec=檢視 %1

; *** "Setup Needs the Next Disk" stuff
ChangeDiskTitle=安裝程式需要下一張磁片
SelectDiskLabel2=請插入磁片 %1，然後按一下「確定」。%n%n如果此磁片上的檔案位於下方所顯示以外的資料夾，請輸入正確的路徑，或按一下「瀏覽」。
PathLabel=路徑(&P)：
FileNotInDir2=在「%2」中找不到檔案「%1」。請插入正確的磁片，或選取其他資料夾。
SelectDirectoryLabel=請指定下一張磁片的位置。

; *** Installation phase messages
SetupAborted=安裝未完成。%n%n請修正這個問題，然後再次執行安裝程式。
AbortRetryIgnoreSelectAction=選取動作
AbortRetryIgnoreRetry=再試一次(&T)
AbortRetryIgnoreIgnore=忽略錯誤並繼續(&I)
AbortRetryIgnoreCancel=取消安裝
RetryCancelSelectAction=選取動作
RetryCancelRetry=再試一次(&T)
RetryCancelCancel=取消

; *** Installation status messages
StatusClosingApplications=正在關閉應用程式...
StatusCreateDirs=正在建立資料夾...
StatusExtractFiles=正在解壓縮檔案...
StatusDownloadFiles=正在下載檔案...
StatusCreateIcons=正在建立捷徑...
StatusCreateIniEntries=正在建立 INI 項目...
StatusCreateRegistryEntries=正在建立登錄項目...
StatusRegisterFiles=正在登錄檔案...
StatusSavingUninstall=正在儲存解除安裝資訊...
StatusRunProgram=正在完成安裝...
StatusRestartingApplications=正在重新啟動應用程式...
StatusRollback=正在復原變更...

; *** Misc. errors
ErrorInternal2=內部錯誤：%1
ErrorFunctionFailedNoCode=%1 失敗
ErrorFunctionFailed=%1 失敗；代碼 %2
ErrorFunctionFailedWithMessage=%1 失敗；代碼 %2。%n%3
ErrorExecutingProgram=無法執行檔案：%n%1

; *** Registry errors
ErrorRegOpenKey=開啟登錄機碼時發生錯誤：%n%1\%2
ErrorRegCreateKey=建立登錄機碼時發生錯誤：%n%1\%2
ErrorRegWriteKey=寫入登錄機碼時發生錯誤：%n%1\%2

; *** INI errors
ErrorIniEntry=在檔案「%1」中建立 INI 項目時發生錯誤。

; *** File copying errors
FileAbortRetryIgnoreSkipNotRecommended=略過此檔案(&S) (不建議)
FileAbortRetryIgnoreIgnoreNotRecommended=忽略錯誤並繼續(&I) (不建議)
SourceIsCorrupted=來源檔案已損毀
SourceDoesntExist=來源檔案「%1」不存在
SourceVerificationFailed=來源檔案驗證失敗：%1
VerificationSignatureDoesntExist=簽章檔案「%1」不存在
VerificationSignatureInvalid=簽章檔案「%1」無效
VerificationKeyNotFound=簽章檔案「%1」使用了未知的金鑰
VerificationFileNameIncorrect=檔案名稱不正確
VerificationFileTagIncorrect=檔案標籤不正確
VerificationFileSizeIncorrect=檔案大小不正確
VerificationFileHashIncorrect=檔案雜湊不正確
ExistingFileReadOnly2=無法取代現有的檔案，因為它標示為唯讀。
ExistingFileReadOnlyRetry=移除唯讀屬性並再試一次(&R)
ExistingFileReadOnlyKeepExisting=保留現有的檔案(&K)
ErrorReadingExistingDest=嘗試讀取現有的檔案時發生錯誤：
FileExistsSelectAction=選取動作
FileExists2=檔案已經存在。
FileExistsOverwriteExisting=覆寫現有的檔案(&O)
FileExistsKeepExisting=保留現有的檔案(&K)
FileExistsOverwriteOrKeepAll=對接下來的衝突執行相同動作(&D)
ExistingFileNewerSelectAction=選取動作
ExistingFileNewer2=現有的檔案比安裝程式要安裝的檔案還新。
ExistingFileNewerOverwriteExisting=覆寫現有的檔案(&O)
ExistingFileNewerKeepExisting=保留現有的檔案(&K) (建議)
ExistingFileNewerOverwriteOrKeepAll=對接下來的衝突執行相同動作(&D)
ErrorChangingAttr=嘗試變更現有檔案的屬性時發生錯誤：
ErrorCreatingTemp=嘗試在目的地資料夾中建立檔案時發生錯誤：
ErrorReadingSource=嘗試讀取來源檔案時發生錯誤：
ErrorCopying=嘗試複製檔案時發生錯誤：
ErrorDownloading=嘗試下載檔案時發生錯誤：
ErrorExtracting=嘗試解壓縮封存檔時發生錯誤：
ErrorReplacingExistingFile=嘗試取代現有的檔案時發生錯誤：
ErrorRestartReplace=RestartReplace 失敗：
ErrorRenamingTemp=嘗試重新命名目的地資料夾中的檔案時發生錯誤：
ErrorRegisterServer=無法登錄 DLL/OCX：%1
ErrorRegSvr32Failed=RegSvr32 失敗，結束代碼 %1
ErrorRegisterTypeLib=無法登錄型別程式庫：%1

; *** Uninstall display name markings
; used for example as 'My Program (32-bit)'
UninstallDisplayNameMark=%1 (%2)
; used for example as 'My Program (32-bit, All users)'
UninstallDisplayNameMarks=%1 (%2, %3)
UninstallDisplayNameMark32Bit=32 位元
UninstallDisplayNameMark64Bit=64 位元
UninstallDisplayNameMarkAllUsers=所有使用者
UninstallDisplayNameMarkCurrentUser=目前使用者

; *** Post-installation errors
ErrorOpeningReadme=嘗試開啟讀我檔案時發生錯誤。
ErrorRestartingComputer=安裝程式無法重新啟動電腦。請手動重新啟動。

; *** Uninstaller messages
UninstallNotFound=檔案「%1」不存在。無法解除安裝。
UninstallOpenError=無法開啟檔案「%1」。無法解除安裝
UninstallUnsupportedVer=此版本的解除安裝程式無法辨識解除安裝記錄檔「%1」的格式。無法解除安裝
UninstallUnknownEntry=在解除安裝記錄中發現未知的項目 (%1)
ConfirmUninstall=確定要完全移除 %1 及其所有元件嗎？
UninstallOnlyOnWin64=此安裝只能在 64 位元 Windows 上解除安裝。
OnlyAdminCanUninstall=此安裝只能由具有系統管理員權限的使用者解除安裝。
UninstallStatusLabel=正在從您的電腦移除 %1，請稍候。
UninstalledAll=已成功從您的電腦移除 %1。
UninstalledMost=%1 解除安裝完成。%n%n有些項目無法移除，您可以手動移除這些項目。
UninstalledAndNeedsRestart=必須重新啟動您的電腦，才能完成 %1 的解除安裝。%n%n要立即重新啟動嗎？
UninstallDataCorrupted=檔案「%1」已損毀。無法解除安裝

; *** Uninstallation phase messages
ConfirmDeleteSharedFileTitle=要移除共用檔案嗎？
ConfirmDeleteSharedFile2=系統指出下列共用檔案已不再由任何程式使用。要讓解除安裝程式移除此共用檔案嗎？%n%n如果仍有程式在使用此檔案而將它移除，這些程式可能無法正常運作。如果不確定，請選擇「否」。將檔案保留在系統上並不會造成任何損害。
SharedFileNameLabel=檔案名稱：
SharedFileLocationLabel=位置：
WizardUninstalling=解除安裝狀態
StatusUninstalling=正在解除安裝 %1...

; *** Shutdown block reasons
ShutdownBlockReasonInstallingApp=正在安裝 %1。
ShutdownBlockReasonUninstallingApp=正在解除安裝 %1。

; The custom messages below aren't used by Setup itself, but if you make
; use of them in your scripts, you'll want to translate them.

[CustomMessages]

NameAndVersion=%1 版本 %2
AdditionalIcons=其他捷徑：
CreateDesktopIcon=建立桌面捷徑(&D)
CreateQuickLaunchIcon=建立快速啟動捷徑(&Q)
ProgramOnTheWeb=%1 網站
UninstallProgram=解除安裝 %1
LaunchProgram=啟動 %1
AssocFileExtension=將 %1 與 %2 副檔名建立關聯(&A)
AssocingFileExtension=正在將 %1 與 %2 副檔名建立關聯...
AutoStartProgramGroupDescription=啟動：
AutoStartProgram=自動啟動 %1
AddonHostProgramNotFound=在您選取的資料夾中找不到 %1。%n%n仍要繼續嗎？

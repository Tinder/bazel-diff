package com.bazel_diff;

import org.junit.Before;
import org.junit.Rule;
import org.junit.Test;
import org.mockito.*;
import org.mockito.junit.*;

import java.io.IOException;
import java.security.NoSuchAlgorithmException;
import java.util.*;

import static org.junit.Assert.*;
import static org.mockito.Mockito.*;

public class TargetHashingClientImplTests {

    @Mock
    BazelClient bazelClientMock;

    List<BazelTarget> defaultTargets;

    @Rule
    public MockitoRule mockitoRule = MockitoJUnit.rule();

    @Before
    public void setUp() throws Exception {
        defaultTargets = new ArrayList<>();
        defaultTargets.add(createRuleTarget("rule1", new ArrayList<>(), "rule1Digest"));
        defaultTargets.add(createRuleTarget("rule2", new ArrayList<>(), "rule2Digest"));
    }

    @Test
    public void hashAllBazelTargets_noTargets() throws IOException {
        when(bazelClientMock.queryAllTargets()).thenReturn(new ArrayList<>());
        TargetHashingClientImpl client = new TargetHashingClientImpl(bazelClientMock);
        try {
            Map<String, String> hash = client.hashAllBazelTargets(new HashSet<>());
            assertEquals(hash.size(), 0);
        } catch (IOException e) {
            assertTrue(false);
        } catch (NoSuchAlgorithmException e) {
            assertTrue(false);
        }
    }

    @Test
    public void hashAllBazelTargets_ruleTargets() throws IOException {
        when(bazelClientMock.queryAllTargets()).thenReturn(defaultTargets);
        TargetHashingClientImpl client = new TargetHashingClientImpl(bazelClientMock);
        try {
            Map<String, String> hash = client.hashAllBazelTargets(new HashSet<>());
            assertEquals(2, hash.size());
            assertEquals("2c963f7c06bc1cead7e3b4759e1472383d4469fc3238dc42f8848190887b4775", hash.get("rule1"));
            assertEquals("bdc1abd0a07103cea34199a9c0d1020619136ff90fb88dcc3a8f873c811c1fe9", hash.get("rule2"));
        } catch (IOException | NoSuchAlgorithmException e) {
            fail(e.getMessage());
        }
    }

    @Test
    public void hashAllBazelTargets_ruleTargets_ruleInputs() throws IOException, NoSuchAlgorithmException {
        List<String> ruleInputs = new ArrayList<>();
        ruleInputs.add("rule1");
        BazelTarget rule3 = createRuleTarget("rule3", ruleInputs, "digest");
        defaultTargets.add(rule3);
        when(bazelClientMock.queryAllTargets()).thenReturn(defaultTargets);
        TargetHashingClientImpl client = new TargetHashingClientImpl(bazelClientMock);
        try {
            Map<String, String> hash = client.hashAllBazelTargets(new HashSet<>());
            assertEquals(3, hash.size());
            assertEquals("2c963f7c06bc1cead7e3b4759e1472383d4469fc3238dc42f8848190887b4775", hash.get("rule1"));
            assertEquals("bdc1abd0a07103cea34199a9c0d1020619136ff90fb88dcc3a8f873c811c1fe9", hash.get("rule2"));
            assertEquals("0f34d9fff9902c8e5915d82ded7a42f714c80bc55dfc1558f743fe01c5821923", hash.get("rule3"));
        } catch (IOException | NoSuchAlgorithmException e) {
            fail(e.getMessage());
        }
    }

    @Test
    public void hashAllBazelTargets_sourceTargets_unmodifiedSources() throws IOException {
        defaultTargets.add(createSourceTarget("sourceFile1"));
        when(bazelClientMock.queryAllTargets()).thenReturn(defaultTargets);
        TargetHashingClientImpl client = new TargetHashingClientImpl(bazelClientMock);
        try {
            Map<String, String> hash = client.hashAllBazelTargets(new HashSet<>());
            assertEquals(3, hash.size());
            assertEquals("2c963f7c06bc1cead7e3b4759e1472383d4469fc3238dc42f8848190887b4775", hash.get("rule1"));
            assertEquals("bdc1abd0a07103cea34199a9c0d1020619136ff90fb88dcc3a8f873c811c1fe9", hash.get("rule2"));
            assertEquals("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855", hash.get("sourceFile1"));
        } catch (IOException | NoSuchAlgorithmException e) {
            fail(e.getMessage());
        }
    }

    @Test
    public void hashAllBazelTargets_sourceTargets_modifiedSources() throws IOException, NoSuchAlgorithmException {
        createSourceTarget("sourceFile1");
        defaultTargets.add(createSourceTarget("sourceFile1"));
        Set<BazelSourceFileTarget> modifiedFileTargets = new HashSet<>();
        modifiedFileTargets.add(createSourceFileTarget("sourceFile1", "digest"));
        when(bazelClientMock.queryAllTargets()).thenReturn(defaultTargets);
        when(bazelClientMock.convertFilepathsToSourceTargets(anySet())).thenReturn(modifiedFileTargets);
        TargetHashingClientImpl client = new TargetHashingClientImpl(bazelClientMock);
        try {
            Map<String, String> hash = client.hashAllBazelTargets(new HashSet<>());
            assertEquals(3, hash.size());
            assertEquals("2c963f7c06bc1cead7e3b4759e1472383d4469fc3238dc42f8848190887b4775", hash.get("rule1"));
            assertEquals("bdc1abd0a07103cea34199a9c0d1020619136ff90fb88dcc3a8f873c811c1fe9", hash.get("rule2"));
            assertEquals("0bf474896363505e5ea5e5d6ace8ebfb13a760a409b1fb467d428fc716f9f284", hash.get("sourceFile1"));
        } catch (IOException | NoSuchAlgorithmException e) {
            fail(e.getMessage());
        }
    }

    @Test
    public void getImpactedTargets() {
        TargetHashingClientImpl client = new TargetHashingClientImpl(bazelClientMock);
        Map<String, String> hash1 = new HashMap<>();
        hash1.put("rule1", "rule1hash");
        hash1.put("rule2", "rule2hash");
        Map<String, String> hash2 = new HashMap<>();
        hash2.put("rule1", "differentrule1hash");
        hash2.put("rule2", "rule2hash");
        hash2.put("rule3", "rule3hash");
        Set<String> impactedTargets = client.getImpactedTargets(hash1, hash2);
        Set<String> expectedSet = new HashSet<>();
        expectedSet.add("rule1");
        expectedSet.add("rule3");
        assertEquals(expectedSet, impactedTargets);
    }

    @Test
    public void getImpactedTestTargets() throws IOException {
        when(bazelClientMock.queryForImpactedTestTargets(anySet())).thenReturn(
                new HashSet<>(Arrays.asList("rule1test", "rule3test", "someothertest"))
        );
        TargetHashingClientImpl client = new TargetHashingClientImpl(bazelClientMock);
        Map<String, String> hash1 = new HashMap<>();
        hash1.put("rule1", "rule1hash");
        hash1.put("rule2", "rule2hash");
        Map<String, String> hash2 = new HashMap<>();
        hash2.put("rule1", "differentrule1hash");
        hash2.put("rule2", "rule2hash");
        hash2.put("rule3", "rule3hash");
        Set <String> impactedTestTargets = client.getImpactedTestTargets(hash1, hash2);
        Set<String> expectedSet = new HashSet<>();
        expectedSet.add("rule1test");
        expectedSet.add("someothertest");
        expectedSet.add("rule3test");
        assertEquals(expectedSet, impactedTestTargets);
    }

    private BazelTarget createRuleTarget(String ruleName, List<String> ruleInputs, String ruleDigest) throws NoSuchAlgorithmException {
        BazelTarget target = mock(BazelTarget.class);
        BazelRule rule = mock(BazelRule.class);
        when(rule.getName()).thenReturn(ruleName);
        when(rule.getRuleInputList()).thenReturn(ruleInputs);
        when(rule.getDigest()).thenReturn(ruleDigest.getBytes());
        when(target.getRule()).thenReturn(rule);
        when(target.hasRule()).thenReturn(true);
        when(target.hasSourceFile()).thenReturn(false);
        return target;
    }

    private BazelTarget createSourceTarget(String name) {
        BazelTarget target = mock(BazelTarget.class);
        when(target.hasSourceFile()).thenReturn(true);
        when(target.hasRule()).thenReturn(false);
        when(target.getRule()).thenReturn(null);
        when(target.getSourceFileName()).thenReturn(name);
        return target;
    }

    private BazelSourceFileTarget createSourceFileTarget(String name, String digest) {
        BazelSourceFileTarget target = mock(BazelSourceFileTarget.class);
        when(target.getName()).thenReturn(name);
        when(target.getDigest()).thenReturn(digest.getBytes());
        return target;
    }
}
